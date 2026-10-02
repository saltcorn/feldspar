//! What a module host is, from the outside: [`ModuleHost`], the manifest a
//! module answers a load with, and the script that runs inside it.
//!
//! A module used to run in a `node` child process behind a newline-JSON pipe.
//! It runs on a **Deno worker in this process** now ([`crate::deno`]) — the same
//! V8 the code pool already links, with `node` off the server's list of runtime
//! requirements and a permission set available to a module's worker, which
//! `node` had no way to offer. This module is what the rest of the server sees
//! of that: the same four calls, the same manifest, and none of the change.
//!
//! **Sandboxed**: a module's worker is built with the permission set on its
//! `_fd_modules` row — closed unless an admin granted something — and modules
//! are pinned to workers by that set, because a `PermissionsContainer` belongs
//! to an isolate (§2).
//!
//! **Lazily started**: a deployment with no modules never builds an isolate.
//! **Restarted on death**: a module that calls `process.exit()`, spins past its
//! JS slice or exhausts the heap ends *its own worker*; every call in flight on
//! it is failed by name, and the next call gets a fresh worker with every load
//! replayed into it, so the module set survives a crash without the caller
//! knowing there was one.
//!
//! A call is one V8 function call into [`HOST_SCRIPT`]'s entry point, carrying
//! an `id` the answer carries back — so many calls are in flight at once and a
//! slow module's action does not hold anybody else's. The modules' own
//! `console.log` goes straight into [`sc_log`], tagged with the module's name.

use std::path::{Path, PathBuf};

use sc_error::{Error, Result};
use sc_expr::{CodeHosts, SchemaSnapshot};
use sc_viewpattern::ViewSnapshot;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::permissions::ModulePermissions;

/// What one module call may reach while it runs, and what its `Table` knows
/// without reaching for anything (TODO "the v1 `Table` API" §3).
///
/// The five host surfaces a code body reaches, plus the schema snapshot — the
/// same pair a [`sc_expr::CodeCall`] carries, and built in the same place: a
/// module's action gets them from `sc_core_actions::CodeSurfaces`, so its write
/// carries the event's caller and this trigger's chain exactly as `run_js_code`'s
/// does.
///
/// [`Default`] is the honest answer for every call that has **nobody's**
/// authority to lend: a load (`onLoad`, a configuration workflow), a module
/// function hoisted into a formula, a table provider called from inside a query.
/// None of those has a caller with a `CodeHosts` borrowed on its stack, and a
/// `Table` reached from one of them is refused by name rather than answering
/// nothing.
#[derive(Default)]
pub struct CallHosts<'a> {
    /// The five surfaces — of which this milestone's `Table` speaks two: the
    /// tables and the triggers.
    pub hosts: CodeHosts<'a>,
    /// This server's tables as the guest sees them without asking — what v1's
    /// synchronous `Table.findOne` is answered from.
    pub schema: Option<&'a SchemaSnapshot>,
    /// An application's views and pages, when the call renders one of them —
    /// what v1's synchronous `View.findOne` is answered from (TODO "Saltcorn
    /// UI" §4). Sent to a worker once per generation, like the schema.
    pub views: Option<&'a ViewSnapshot>,
}

impl<'a> CallHosts<'a> {
    /// The surfaces and the schema of one run, as
    /// `sc_core_actions::code_body::Hosts` supplies them.
    #[must_use]
    pub fn new(hosts: CodeHosts<'a>, schema: Option<&'a SchemaSnapshot>) -> CallHosts<'a> {
        CallHosts {
            hosts,
            schema,
            views: None,
        }
    }

    /// The same, carrying an application's view snapshot.
    #[must_use]
    pub fn with_views(mut self, views: &'a ViewSnapshot) -> CallHosts<'a> {
        self.views = Some(views);
        self
    }
}

/// The host script's own half, before the shared v1 API is put in front of it.
///
/// `pub(crate)` because [`crate::deno`] is what writes and evaluates it: it is
/// the JavaScript half of the host, and the only thing that runs it is the
/// worker. Which is also why a build without that feature has nothing that
/// reads it but this module's own tests.
#[cfg_attr(
    not(any(feature = "deno-host", test)),
    expect(dead_code, reason = "no runtime to run it")
)]
pub(crate) const HOST_SCRIPT: &str = include_str!("js/module-host.mjs");

/// What is actually written into the modules root at every worker start: the
/// shared v1 `Table`/`Field` source, then this host's own script.
///
/// **One source, two hosts** (TODO "the v1 `Table` API" §1). `sc_expr::V1_API_JS`
/// is the same text compiled into the code isolates' prelude, so the `Table` a
/// `run_js_code` body gets and the `Table` an installed v1 plugin gets are the
/// same translation of v1's `Where` vocabulary. Two implementations that agreed
/// today would disagree by the third bug fixed in one of them.
///
/// Concatenated rather than imported because the worker evaluates **one** main
/// module and `v1_api.js` is a script — an IIFE that defines `__scMakeV1Api` on
/// the global — so putting it first is all the wiring there is.
#[cfg_attr(
    not(any(feature = "deno-host", test)),
    expect(dead_code, reason = "no runtime to run it")
)]
pub(crate) fn host_script() -> String {
    format!("{}\n{HOST_SCRIPT}", sc_expr::V1_API_JS)
}

/// What the host script is called on disk.
pub const HOST_SCRIPT_NAME: &str = "module-host.mjs";

pub use crate::bounds::DEFAULT_CALL_TIMEOUT;

/// One action, as the module declared it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActionManifest {
    /// The name the action is registered under — v1's own, unqualified.
    pub name: String,
    /// The module's one-line description, if it gave one.
    #[serde(default)]
    pub description: String,
    /// Whether the action needs a row to act on.
    #[serde(default, rename = "requireRow")]
    pub require_row: bool,
    /// v1 `configFields`, as the module declared them — translated by
    /// [`crate::spec`], never interpreted here.
    #[serde(default, rename = "configFields")]
    pub config_fields: Vec<Json>,
}

/// One argument of a module function, as v1 declares it.
///
/// v1's own `arguments: [{ name, type }]` vocabulary, kept rather than
/// reinvented: `type` is a v1 field type name (`String`, `Integer`, `Object`),
/// which is the same vocabulary [`crate::spec`] already translates. Absent when
/// the module did not say — a v1 function may declare nothing at all, and
/// `null` is the honest answer rather than a guess.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FunctionArg {
    /// The argument's name, as the signature shows it.
    pub name: String,
    /// The v1 type name, when the module declared one.
    #[serde(default, rename = "type")]
    pub type_name: Option<String>,
}

/// One function, as the module declared it (§4a).
///
/// A v1 plugin supplies `functions` beside `actions`, and v1 makes them
/// "available to formulas and code actions". The three shapes v1 allows — a bare
/// function, a `{ run, isAsync, description, arguments }` object, and a function
/// of the module's own configuration — are resolved in the host script; what
/// arrives here is the one shape.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FunctionManifest {
    /// The name the function is registered under — v1's own, unqualified. Two
    /// modules may each supply the same one; nothing here disambiguates them,
    /// because which module is meant is the *caller's* question.
    pub name: String,
    /// The module's one-line description, if it gave one.
    #[serde(default)]
    pub description: String,
    /// Whether v1 itself treated this function as awaitable. It does not decide
    /// how the function is *called* — everything crosses the seam awaited — but
    /// it is what a signature in the code editor says, and it is v1's word.
    #[serde(default, rename = "isAsync")]
    pub is_async: bool,
    /// The declared signature, when the module declared one.
    #[serde(default)]
    pub arguments: Vec<FunctionArg>,
}

/// One **table provider** a module supplies (§8.3).
///
/// v1's `table_providers` key: a virtual table whose rows the module produces.
/// What arrives here is its name and the fields of its own
/// `configuration_workflow`, flattened by the host script exactly as a module's
/// own settings are — the provider that serves the rows stays in the worker, and
/// what crosses is what the admin has to fill in.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct TableProviderManifest {
    /// The provider's own name — `RSS feed`. v1's, unqualified; the module it
    /// came from is what disambiguates it.
    pub name: String,
    /// v1 `configFields`, as the provider's configuration workflow declared
    /// them — translated by [`crate::spec`], never interpreted here.
    #[serde(default)]
    pub config_fields: Vec<Json>,
}

/// One **model provider** a module supplies (TODO "Predictive models" §14).
///
/// A module exports `modelproviders` beside its `actions` and `table_providers`:
///
/// ```js
/// modelproviders: {
///   ridge: {
///     description: "Linear regression with an L2 penalty",
///     configuration_workflow,                     // or `config_fields: [...]`
///     hyperparameters: [{ name: "alpha", type: "Float", default: 1 }],
///     outcome: { kind: "regression", label: "label" },
///     standardise: true,
///     fit: async ({ frame, configuration, hyperparameters }) => ({ state, parameters }),
///     predict: async ({ state, frame }) => [1.2, 3.4],
///   },
/// }
/// ```
///
/// What crosses is the **declaration**: the name, the two field sets and what a
/// fit of it produces. `fit` and `predict` stay in the worker, and reach it
/// again through [`ModuleHost::model_fit`] and [`ModuleHost::model_predict`].
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelProviderManifest {
    /// The name it is registered and stored under — `ridge`. The module it came
    /// from is what disambiguates two of them.
    pub name: String,
    /// One line for the provider picker.
    #[serde(default)]
    pub description: String,
    /// Its settings, as v1 `configFields` — translated by [`crate::spec`], never
    /// interpreted here.
    #[serde(default)]
    pub config_fields: Vec<Json>,
    /// Its hyperparameters, in the same shape. A model stores a value or a
    /// **list** of values per hyperparameter, and a fit runs the grid.
    #[serde(default)]
    pub hyperparameters: Vec<Json>,
    /// What a fit of it produces, as `sc_model::OutcomeSpec`'s JSON.
    ///
    /// Carried as JSON rather than as the typed value so that a module with one
    /// mis-declared provider is a module with one mis-declared provider: it is
    /// read (and reported, by the host script that loaded it) where the provider
    /// set is built, not while the manifest is being parsed — which would lose
    /// the module's actions to somebody's typo.
    #[serde(default)]
    pub outcome: Json,
    /// Whether the host should standardise the numeric features before handing
    /// them over. A declaration, because the constants are stored on the
    /// instance and applied again at predict time.
    #[serde(default)]
    pub standardise: bool,
    /// What a fit of it shows (analytics TODO A3.2): a list of
    /// `sc_model::OutputDecl`'s JSON — parameter tables, the metrics, and plot
    /// specs over the fit's output data. Absent for the standard outputs of
    /// its outcome. JSON for the reason `outcome` is.
    #[serde(default)]
    pub outputs: Json,
}

/// One **stream provider** a module supplies (TODO "Streams" §12).
///
/// A module exports `streamproviders` beside its `actions`, `table_providers`
/// and `modelproviders`:
///
/// ```js
/// streamproviders: {
///   poll_feed: {
///     description: "An RSS feed, polled",
///     config_fields: [{ name: "url", type: "String", required: true },
///                     { name: "interval_s", type: "Integer", default: 60 }],
///     element_type: ({ configuration }) => ({ kind: "json", keys: [ … ] }),
///     poll: async ({ configuration, cursor }) => ({ elements: [ … ], cursor: "…" }),
///   },
/// }
/// ```
///
/// **Poll, not push**, and that is the one place a module provider is shaped
/// differently from a Rust one: a module call is request/response on a Deno
/// worker, there is no channel from a worker back into the host, and
/// `sc_stream::PollingProvider` is what supplies the loop. What crosses here is
/// only the **declaration** — the name, the description and the settings;
/// `element_type` and `poll` stay in the worker and are reached again through
/// [`ModuleHost::stream_element_type`] and [`ModuleHost::stream_poll`].
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct StreamProviderManifest {
    /// The name it is registered and stored under — `poll_feed`. Shares one
    /// namespace with the built-in providers and every other module's, so a
    /// duplicate is refused by the registry naming both sources.
    pub name: String,
    /// One line for the provider picker.
    #[serde(default)]
    pub description: String,
    /// What the picker calls it. Defaults to the name on this side.
    #[serde(default)]
    pub label: Option<String>,
    /// Its settings, as v1 `configFields` — translated by [`crate::spec`],
    /// never interpreted here.
    #[serde(default)]
    pub config_fields: Vec<Json>,
}

/// One **application framework** a module supplies (§13.3, §15.1).
///
/// A module exports `frameworks` beside its `actions` and `table_providers`:
///
/// ```js
/// frameworks: {
///   vue: {
///     label: "Vue",
///     description: "A Vue 3 + Vite project, scaffolded and built for you.",
///     config_fields: [{ name: "store", type: "String", required: true },
///                     { name: "project", type: "String", default: "" }],
///     build: {
///       store: "{{ store }}", source: "{{ project }}",
///       output: "{{ project }}/dist", command: "npm run build",
///       install: { command: "npm install", marker: "node_modules" },
///       runtime: "{{ project }}/src/feldspar", client: "client.ts",
///     },
///     csp: { "img-src": ["'self'", "data:"] },
///     builder_prompt: "You maintain {{ app }} …",
///     checks: ["typecheck"],
///     scaffold: async (ctx) => [{ path: "package.json", contents: "…" }],
///     runtime: async (ctx) => [{ path: `${ctx.runtime}/composables.ts`, contents: "…" }],
///   },
/// }
/// ```
///
/// **What crosses is the declaration**, and that is the whole design decision:
/// every question the admin UI asks a framework — its settings, its default CSP,
/// where its source is — is asked synchronously, on the path that renders a form
/// or resolves a build, and none of the answers depends on anything the module
/// learns at run time. So they cross once, here, and are installed as values
/// (`sc_app::FrameworkDecl`). The two functions stay in the worker and are
/// reached again through [`ModuleHost::framework_files`], on the path that was
/// already asynchronous.
///
/// The paths are `{{ }}` templates over the framework's own settings, parsed by
/// `sc-expr`'s one template parser — the same one an email subject goes through.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FrameworkManifest {
    /// The registry key and what an application's framework reference stores —
    /// `vue`. Unqualified, sharing one namespace with the built-ins and with
    /// every other module's, exactly as an action's name does.
    pub name: String,
    /// The human name the framework picker shows.
    #[serde(default)]
    pub label: String,
    /// One sentence: what it does for the admin, and what it asks in return.
    #[serde(default)]
    pub description: String,
    /// Its settings, as v1 `configFields` — translated by [`crate::spec`], never
    /// interpreted here.
    #[serde(default)]
    pub config_fields: Vec<Json>,
    /// Where its source is and how it builds, as templates. Carried as JSON for
    /// the reason a model provider's `outcome` is: a module with one
    /// mis-declared framework is a module with one mis-declared framework, and
    /// reading it where the framework set is built keeps somebody's typo from
    /// costing the module its actions.
    #[serde(default)]
    pub build: Json,
    /// The widenings its output needs on top of the strict CSP baseline:
    /// directive name → source list.
    #[serde(default)]
    pub csp: Json,
    /// Its builder agent's system prompt, as a template. Empty for a framework
    /// that declares no builder agent.
    #[serde(default)]
    pub builder_prompt: String,
    /// The `package.json` scripts its builder agent checks with, in order —
    /// `["typecheck"]`. The application build always follows them.
    #[serde(default)]
    pub checks: Vec<String>,
    /// Whether it exported a `scaffold` function — whether an application of it
    /// has a project Saltcorn writes, or one the admin brought.
    #[serde(default)]
    pub scaffolds: bool,
}

/// An entity type the module exports and this version does not load.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UnsupportedEntity {
    /// The plugin key — `types`, `fieldviews`, `eventTypes`.
    pub key: String,
    /// How many of them, when that can be told without running the module's
    /// code.
    #[serde(default)]
    pub count: Option<u64>,
}

/// What a module turned out to supply.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModuleManifest {
    /// The package name it was loaded under.
    pub name: String,
    /// v1's `sc_plugin_api_version`, when it declares one.
    #[serde(default)]
    pub api_version: Option<u64>,
    /// v1's `plugin_name`, when it declares one.
    #[serde(default)]
    pub plugin_name: Option<String>,
    /// The actions it supplies.
    #[serde(default)]
    pub actions: Vec<ActionManifest>,
    /// The functions it supplies (§4a) — what a code body calls through
    /// `modfn` and what a formula hoists.
    #[serde(default)]
    pub functions: Vec<FunctionManifest>,
    /// The table providers it supplies (§8.3) — what the "new table" screen
    /// offers as a source beside a database.
    #[serde(default)]
    pub table_providers: Vec<TableProviderManifest>,
    /// The model providers it supplies — what the model form offers beside the
    /// built-in regressions.
    #[serde(default)]
    pub model_providers: Vec<ModelProviderManifest>,
    /// The stream providers it supplies (TODO "Streams" §12) — what the
    /// Streams form offers beside the built-in MQTT one.
    #[serde(default)]
    pub stream_providers: Vec<StreamProviderManifest>,
    /// The application frameworks it supplies (§13.3) — what the application
    /// form offers beside `react` and `code`.
    #[serde(default)]
    pub frameworks: Vec<FrameworkManifest>,
    /// The view patterns it supplies (TODO "Saltcorn UI" §6) — v1's
    /// `viewtemplates`, described as data. The functions stay in the worker, in
    /// the view runtime's registry. After [`ModuleSet::load`](crate::ModuleSet)
    /// only the ones whose names were free are left here; a clash is an issue.
    #[serde(default)]
    pub view_patterns: Vec<sc_viewpattern::PatternManifest>,
    /// v1's `headers`: the scripts and stylesheets a document rendering its
    /// patterns wants (11.2).
    #[serde(default)]
    pub headers: Vec<sc_viewpattern::PluginHeader>,
    /// The fields of its `configuration_workflow`'s forms, flattened (§5).
    #[serde(default)]
    pub config_fields: Vec<Json>,
    /// What it also supplies and this version does not load (§6).
    #[serde(default)]
    pub unsupported: Vec<UnsupportedEntity>,
    /// What went wrong that was not fatal — a step's form that would not build,
    /// an action whose `configFields` threw.
    #[serde(default)]
    pub issues: Vec<String>,
}

/// A module's own failure, as this system's error.
///
/// An **Application** error (§16): the fault is in the module or in how it was
/// configured, not in Saltcorn, and the admin who installed it is the one who
/// can act.
///
/// `pub(crate)`: [`crate::deno`] is where a module's throw arrives.
#[cfg_attr(
    not(any(feature = "deno-host", test)),
    expect(dead_code, reason = "no runtime for a module to throw on")
)]
pub(crate) fn module_error(message: &str) -> Error {
    Error::config(if message.is_empty() {
        "the module failed without saying why".to_owned()
    } else {
        message.to_owned()
    })
}

/// The module host: the worker pool a module's JavaScript runs on, and the four
/// calls the rest of the server makes of it.
///
/// A façade over [`crate::deno::DenoModuleHost`], and deliberately a thin one —
/// what it exists for is that `ModuleServices`, [`ModuleAction`](crate::action),
/// the five module endpoints and the four-step reload name a *module host* and
/// not a runtime. It is also where a build without the `deno-host` feature
/// arrives: the crate goes on building and testing without `deno_runtime` (which
/// is what keeps 444 lock-file packages and a `libclang` build requirement out
/// of every other crate's test link), and a call on such a build fails saying
/// exactly that rather than pretending.
pub struct ModuleHost {
    root: PathBuf,
    #[cfg(feature = "deno-host")]
    pool: crate::deno::DenoModuleHost,
}

impl ModuleHost {
    /// A host over the modules root, with the default number of workers.
    /// Nothing is built until the first call.
    pub fn new(root: impl Into<PathBuf>) -> ModuleHost {
        ModuleHost::with_workers(root, crate::bounds::DEFAULT_MODULE_WORKERS)
    }

    /// A host of `workers` workers (at least one) over the modules root.
    pub fn with_workers(root: impl Into<PathBuf>, workers: usize) -> ModuleHost {
        let root = root.into();
        #[cfg(not(feature = "deno-host"))]
        let _ = workers;
        ModuleHost {
            #[cfg(feature = "deno-host")]
            pool: crate::deno::DenoModuleHost::with_workers(&root, workers),
            root,
        }
    }

    /// A host whose calls are bounded by `timeout` rather than
    /// [`DEFAULT_CALL_TIMEOUT`].
    #[must_use]
    pub fn with_timeout(self, timeout: std::time::Duration) -> ModuleHost {
        #[cfg(feature = "deno-host")]
        {
            ModuleHost {
                root: self.root,
                pool: self.pool.with_timeout(timeout),
            }
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = timeout;
            self
        }
    }

    /// A host whose workers can load Saltcorn UI's view runtime from `runtime` —
    /// the bundle's `view-runtime.js` — or cannot, for `None`.
    ///
    /// Every worker is told, not only the one views render on: the bundle is
    /// also the **library** a v1 plugin's `require("@saltcorn/markup/tags")` is
    /// answered from, and a plugin granted a host lives on a worker of its own
    /// (TODO "Saltcorn UI" §5).
    #[must_use]
    pub fn with_view_runtime(self, runtime: Option<PathBuf>) -> ModuleHost {
        #[cfg(feature = "deno-host")]
        {
            ModuleHost {
                root: self.root,
                pool: self.pool.with_view_runtime(runtime),
            }
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = runtime;
            self
        }
    }

    /// The modules root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Install the module view patterns — `(module, pattern)`, as
    /// [`ModuleSet::installed_view_patterns`](crate::ModuleSet::installed_view_patterns)
    /// resolves them — into the view runtime's registry, whole (TODO "Saltcorn
    /// UI" 11.1).
    pub fn install_view_patterns(&self, patterns: &[(String, String)]) {
        #[cfg(feature = "deno-host")]
        {
            self.pool.install_view_patterns(patterns);
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = patterns;
        }
    }

    /// The generation of the installed view patterns: what the view runtime's
    /// pattern manifest is cached against.
    pub fn view_patterns_generation(&self) -> u64 {
        #[cfg(feature = "deno-host")]
        {
            self.pool.view_patterns_generation()
        }
        #[cfg(not(feature = "deno-host"))]
        {
            0
        }
    }

    /// Load (or reload) a module from `dir`, with `configuration` as the object
    /// handed to v1's `actions(cfg)` and `permissions` as what its worker may
    /// reach (§2).
    ///
    /// Idempotent, and idempotent **on the same worker**: a reload after a
    /// configuration change replaces the module where its state already is,
    /// rather than leaving a second copy of it somewhere else. A change to the
    /// *permissions* is the exception, and has to be: the set belongs to the
    /// isolate, so the module moves to a worker that grants it — losing whatever
    /// it was holding, exactly as a restart would.
    pub async fn load(
        &self,
        name: &str,
        dir: &Path,
        configuration: &Json,
        permissions: &ModulePermissions,
    ) -> Result<ModuleManifest> {
        #[cfg(feature = "deno-host")]
        {
            self.pool.load(name, dir, configuration, permissions).await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (name, dir, configuration, permissions);
            Err(no_runtime())
        }
    }

    /// Forget a module — after an uninstall, so a restarted worker does not
    /// reload a package that is no longer there.
    pub async fn unload(&self, name: &str) {
        #[cfg(feature = "deno-host")]
        {
            self.pool.unload(name).await;
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = name;
        }
    }

    /// Run one action of one module with v1's argument object, over the surfaces
    /// its caller has.
    ///
    /// The surfaces are what make the v1 `Table` work inside a module (§3): the
    /// action's own asks are served by *this* future, because the hosts are
    /// borrowed on the caller's stack and cannot be sent anywhere.
    pub async fn run(
        &self,
        module: &str,
        action: &str,
        args: Json,
        call: CallHosts<'_>,
    ) -> Result<Json> {
        #[cfg(feature = "deno-host")]
        {
            self.pool.run(module, action, args, call).await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (module, action, args, call);
            Err(no_runtime())
        }
    }

    /// Call one **function** of one module with v1's positional arguments
    /// (§4a).
    ///
    /// The fifth host surface, and routed exactly as [`run`](ModuleHost::run)
    /// is: to the worker the module was loaded on, because a v1 function closes
    /// over what its module built at load time — a `markdown-it`, a
    /// `Nominatim`, the module's own configuration — and that lives in one
    /// place because a module is loaded once.
    pub async fn call(
        &self,
        module: &str,
        function: &str,
        args: Vec<Json>,
        call: CallHosts<'_>,
    ) -> Result<Json> {
        #[cfg(feature = "deno-host")]
        {
            self.pool.call(module, function, args, call).await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (module, function, args, call);
            Err(no_runtime())
        }
    }

    /// The fields one **table provider** presents for one configuration (§8.3).
    ///
    /// Asked on every catalog reload rather than stored: the columns are the
    /// module's answer, so an upgraded package that presents a new column
    /// presents it. Routed like [`run`](ModuleHost::run) and for the same
    /// reason — `fields(cfg)` is a closure the module built at load time.
    pub async fn provider_fields(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
    ) -> Result<Vec<Json>> {
        #[cfg(feature = "deno-host")]
        {
            self.pool
                .provider_fields(module, provider, configuration)
                .await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (module, provider, configuration);
            Err(no_runtime())
        }
    }

    /// One table provider's rows, for v1's `where`/`options` pair.
    ///
    /// The pair is a hint the provider may honour or ignore; the caller applies
    /// the query to the answer either way (`sc_catalog::inmem`).
    pub async fn provider_rows(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
        table: &str,
        filter: &Json,
        options: &Json,
    ) -> Result<Vec<Json>> {
        #[cfg(feature = "deno-host")]
        {
            self.pool
                .provider_rows(module, provider, configuration, table, filter, options)
                .await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (module, provider, configuration, table, filter, options);
            Err(no_runtime())
        }
    }

    /// Which of v1's three write methods `get_table(configuration)` answers.
    ///
    /// v1 has no declaration of writability: the object `get_table` returns
    /// carries `insertRow`/`updateRow`/`deleteRows` or it does not, which is how
    /// `@saltcorn/postgres-tables`'s `read_only` flag works. So this is a
    /// property of the *configuration*, asked once per catalog reload.
    pub async fn provider_writes(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
        table: &str,
    ) -> Result<Json> {
        #[cfg(feature = "deno-host")]
        {
            self.pool
                .provider_writes(module, provider, configuration, table)
                .await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (module, provider, configuration, table);
            Err(no_runtime())
        }
    }

    /// v1's `insertRow(record)`: `{ key }`, the new row's primary key or null.
    pub async fn provider_insert(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
        table: &str,
        record: &Json,
    ) -> Result<Json> {
        #[cfg(feature = "deno-host")]
        {
            self.pool
                .provider_insert(module, provider, configuration, table, record)
                .await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (module, provider, configuration, table, record);
            Err(no_runtime())
        }
    }

    /// v1's `updateRow(record, id)`.
    pub async fn provider_update(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
        table: &str,
        id: &Json,
        record: &Json,
    ) -> Result<Json> {
        #[cfg(feature = "deno-host")]
        {
            self.pool
                .provider_update(module, provider, configuration, table, id, record)
                .await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (module, provider, configuration, table, id, record);
            Err(no_runtime())
        }
    }

    /// v1's `deleteRows(where)`.
    pub async fn provider_delete(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
        table: &str,
        filter: &Json,
    ) -> Result<Json> {
        #[cfg(feature = "deno-host")]
        {
            self.pool
                .provider_delete(module, provider, configuration, table, filter)
                .await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (module, provider, configuration, table, filter);
            Err(no_runtime())
        }
    }

    /// **Fit** one of a module's model providers.
    ///
    /// The frame crosses as **columns, not rows of objects**: a 50 000 × 12
    /// dataset is twelve JSON arrays and not 50 000 objects with the same twelve
    /// keys repeated. Routed like [`run`](ModuleHost::run), because `fit` is a
    /// closure the module built at load time.
    pub async fn model_fit(
        &self,
        module: &str,
        provider: &str,
        frame: &Json,
        configuration: &Json,
        hyperparameters: &Json,
    ) -> Result<Json> {
        #[cfg(feature = "deno-host")]
        {
            self.pool
                .model_fit(module, provider, frame, configuration, hyperparameters)
                .await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (module, provider, frame, configuration, hyperparameters);
            Err(no_runtime())
        }
    }

    /// The files one of a module's frameworks generates for an application —
    /// the whole project (`scaffold`), or the framework's own generated code
    /// (`runtime`).
    ///
    /// The one part of a framework declaration that is a *call* rather than a
    /// value: it depends on the application's tables, its API surface and its
    /// roles, none of which the plugin author knew.
    pub async fn framework_files(
        &self,
        module: &str,
        framework: &str,
        phase: &str,
        context: &Json,
    ) -> Result<Json> {
        #[cfg(feature = "deno-host")]
        {
            self.pool
                .framework_files(module, framework, phase, context)
                .await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (module, framework, phase, context);
            Err(no_runtime())
        }
    }

    /// **Predict** with one, over a frame of any height — a single row is a
    /// frame of one, and batching is what makes a call across this seam worth
    /// its cost.
    pub async fn model_predict(
        &self,
        module: &str,
        provider: &str,
        state: &Json,
        frame: &Json,
    ) -> Result<Json> {
        #[cfg(feature = "deno-host")]
        {
            self.pool
                .model_predict(module, provider, state, frame)
                .await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (module, provider, state, frame);
            Err(no_runtime())
        }
    }

    /// The **element type** one of a module's stream providers declares for a
    /// configuration (TODO "Streams" §12).
    ///
    /// A call rather than a value because GOALS makes the element type a
    /// function of the configuration, and for a module that function is
    /// JavaScript. `sc_stream::PollingProvider` is what asks, and what caches
    /// the answer for the synchronous side of the provider trait.
    pub async fn stream_element_type(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
    ) -> Result<Json> {
        #[cfg(feature = "deno-host")]
        {
            self.pool
                .stream_element_type(module, provider, configuration)
                .await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (module, provider, configuration);
            Err(no_runtime())
        }
    }

    /// **Poll** one of a module's stream providers once.
    ///
    /// `cursor` is whatever the previous poll answered — opaque here and in
    /// `sc-stream`; it is the module's own "where I got to". The answer is
    /// `{ elements, cursor }`, read by `sc_stream::PollAnswer`.
    ///
    /// Routed like [`run`](ModuleHost::run), because `poll` is a closure the
    /// module built at load time and a poll on another isolate would be a poll
    /// against another copy of whatever the module set up.
    pub async fn stream_poll(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
        cursor: &Json,
    ) -> Result<Json> {
        #[cfg(feature = "deno-host")]
        {
            self.pool
                .stream_poll(module, provider, configuration, cursor)
                .await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (module, provider, configuration, cursor);
            Err(no_runtime())
        }
    }

    /// One call into Saltcorn UI's view runtime (TODO "Saltcorn UI" §3): `op` is
    /// the host script's `view_*` operation and `request` its fields.
    ///
    /// Routed to the one worker the built-in runtime is pinned to, over the
    /// surfaces and snapshots `call` carries. [`crate::ModuleViewRuntime`] is
    /// what names these, and the rest of the server names that.
    pub async fn view_call(&self, op: &str, request: Json, call: CallHosts<'_>) -> Result<Json> {
        #[cfg(feature = "deno-host")]
        {
            self.pool.view_call(op, request, call).await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = (op, request, call);
            Err(no_runtime())
        }
    }

    /// Ask the host to say hello — what a test and a diagnostics screen use to
    /// find out whether the pool starts at all.
    pub async fn ping(&self) -> Result<Json> {
        #[cfg(feature = "deno-host")]
        {
            self.pool.ping().await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            Err(no_runtime())
        }
    }

    /// Which worker a module lives on, or `None` if it has never been loaded.
    /// The Modules tab's answer to "where is this thing running".
    pub async fn worker_of(&self, module: &str) -> Option<usize> {
        #[cfg(feature = "deno-host")]
        {
            self.pool.worker_of(module).await
        }
        #[cfg(not(feature = "deno-host"))]
        {
            let _ = module;
            None
        }
    }

    /// Stop every worker and wait for its thread.
    pub async fn shutdown(&self) {
        #[cfg(feature = "deno-host")]
        {
            self.pool.shutdown().await;
        }
    }
}

/// Establish V8's process-wide read-only heap from the module runtime's startup
/// snapshot, before anything else builds an isolate.
///
/// **Call this once, early, in any process that runs both modules and
/// JavaScript** — a server, and any test that touches the two. The order is not
/// a preference: a snapshot-backed worker built after a bare `deno_core`
/// isolate aborts the process inside V8, with no error to catch. See
/// [`crate::deno::prime`] for why, and `sc_expr::set_isolate_prime` for the hook
/// that spares callers from having to sequence it by hand.
///
/// Does nothing in a build without the `deno-host` feature, which has no module
/// runtime to prime.
pub fn prime_v8() {
    #[cfg(feature = "deno-host")]
    {
        crate::deno::prime();
    }
}

/// What a build with no module runtime answers, rather than a silence or a
/// timeout.
#[cfg(not(feature = "deno-host"))]
fn no_runtime() -> Error {
    Error::config(
        "this build of Saltcorn has no module runtime: it was compiled without sc-module's \
         `deno-host` feature, which is what the server binary turns on",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_modules_throw_is_the_admins_problem_and_not_saltcorns() {
        let err = module_error("connect ECONNREFUSED");
        assert!(err.to_string().contains("ECONNREFUSED"), "{err}");
        // The module's fault, not the server's: it is the admin who installed it
        // who can fix it (§16's split).
        assert_eq!(err.kind(), sc_error::ErrorKind::Application);
    }

    #[test]
    fn a_throw_with_nothing_to_say_still_says_something() {
        assert!(module_error("").to_string().contains("without saying why"));
    }

    #[test]
    fn the_host_script_is_carried_in_the_binary() {
        // The script is written from here at every worker start, so a checkout
        // whose `js/` directory moved would be a host that cannot start — worth
        // one assertion that the file is really compiled in.
        assert!(
            HOST_SCRIPT.contains("module-host"),
            "the script looks wrong"
        );
        assert!(HOST_SCRIPT.contains("@saltcorn/"), "the stubs are missing");
    }

    /// **One source, two hosts** (TODO "the v1 `Table` API" §1). The `Table` a
    /// code body gets and the `Table` a v1 plugin gets are the same text, and
    /// this is the assertion that the concatenation really happens: a checkout
    /// where it did not would be a module host whose `require` of
    /// `@saltcorn/data/models/table` answered a façade over a factory that is
    /// not there.
    #[test]
    fn the_written_script_carries_the_shared_v1_api_in_front_of_it() {
        let script = host_script();
        assert!(
            script.contains("__scMakeV1Api"),
            "the shared v1 Table/Field source is missing"
        );
        assert!(
            script.find("__scMakeV1Api") < script.find("globalThis.__scModuleHost"),
            "the factory has to be defined before the host script reads it"
        );
        // And the host's own half is still all there, after it.
        assert!(script.contains("module-host"), "the host script is missing");
        assert!(
            script.contains("__scAnswer"),
            "the ask channel's answer is missing"
        );
    }

    #[test]
    fn the_host_script_speaks_the_seam_and_not_a_pipe() {
        // The three functions [`crate::deno`] installs, and the entry point it
        // calls. If either side is renamed without the other, a worker starts
        // and never answers — so the pair is asserted here, where a rename is
        // one grep away from both.
        for name in [
            "__scModuleHost",
            "__scDone",
            "__scFail",
            "__scLog",
            "__scAsk",
        ] {
            assert!(HOST_SCRIPT.contains(name), "{name} is missing");
        }
        // And nothing is left of the transport: no framing, no stdout, no
        // readline.
        assert!(
            !HOST_SCRIPT.contains("readline"),
            "the newline-JSON loop is still there"
        );
        assert!(
            !HOST_SCRIPT.contains("process.stdout"),
            "the host script still writes to stdout"
        );
    }
}
