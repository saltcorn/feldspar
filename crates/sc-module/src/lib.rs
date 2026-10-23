//! **Modules**: Saltcorn v1 JavaScript plugins, installed with npm and run on a
//! Deno worker in this process (design §15; TODO "Modules", "Modules
//! in-process").
//!
//! A module is an npm package exporting v1's plugin object — `{ actions,
//! configuration_workflow, viewtemplates, … }` — and this milestone reads two of
//! those keys: `actions`, which become [`Action`](sc_action::Action)s in the
//! registry the built-ins live in, and `configuration_workflow`, whose first
//! form is the module's own settings.
//!
//! ## Where a module runs
//!
//! A v1 plugin is a CommonJS Node package **whose dependencies are the point**:
//! `@saltcorn/mqtt` is a wrapper over `async-mqtt` (a TCP/TLS socket),
//! `@saltcorn/proxmox` a wrapper over `proxmox-api` (HTTPS). `sc-expr`'s
//! `CodeRuntime` is a bare V8 with four ops and no module loader — no `require`,
//! no `net`, no `fs` — so a module cannot run there.
//!
//! It runs on an implementation of Node instead: [`deno`], a `deno_runtime`
//! worker thread **in this process**, on the same V8 the code pool already
//! links, entered by an ordinary V8 function call. [`host`] is the façade the
//! rest of the server names — `load`, `run`, `unload`, and the manifest.
//!
//! It used to be a `node` child process behind a newline-JSON pipe, and the
//! change was made for what it makes possible rather than what it saves: `node`
//! is no longer a runtime requirement of a Saltcorn server (npm still installs
//! modules, so it is still a requirement of *installing* one), and a module's
//! worker can be handed a permission set, which `node` had no way to offer.
//!
//! ## The pieces
//!
//! - [`module`] — what a module is: the row, and where its package came from.
//! - [`bundled`] — the modules this server ships with, and installs from itself.
//! - [`store`] — `_fd_modules`, the row's schema and its lifecycle.
//! - [`paths`] — where packages are installed.
//! - [`install`] — npm, and what it turned out to have installed.
//! - [`bounds`] — the four bounds a module call is under, and the pool's size.
//! - [`permissions`] — what a module's worker may reach, and what it may not.
//! - [`host`] — the module host as the rest of the server sees it.
//! - [`deno`] — the in-process worker pool (feature `deno-host`).
//! - [`spec`] — v1's `configFields` translated into this system's `FormField`.
//! - [`action`] — a module's action as an `Action`.
//! - [`functions`] — a module's functions, as `sc-expr`'s fifth host surface.
//! - [`table_providers`] — a module's table providers, as `sc-catalog`'s seam.
//! - [`tables`] — the tables a module declares, created through the schema
//!   editor when it loads (TODO-imap M4).
//! - [`services`] — what a module runs in the background, over its own tables,
//!   and the supervisor that restarts it (TODO-imap M5).
//! - [`frameworks`] — a module's application frameworks, as `sc-app`'s seam.
//! - [`modules`] — the loaded set: every stored module, its actions, its issues.
//! - [`view_runtime`] — Saltcorn UI's view runtime, the built-in
//!   `@feldspar/saltcorn-ui`, as `sc-viewpattern`'s render seam.
//!
//! ## What is *not* here
//!
//! Every other entity type a v1 plugin can export. They are counted and reported
//! ([`ModuleManifest::unsupported`](host::ModuleManifest::unsupported)) so an
//! admin knows what they are not getting, and loading them is a later milestone.

pub mod action;
pub mod bounds;
pub mod bundled;
pub mod consent;
pub mod csp;
#[cfg(feature = "deno-host")]
pub mod deno;
pub mod files;
pub mod frameworks;
pub mod functions;
pub mod host;
pub mod install;
pub mod model_providers;
pub mod module;
pub mod modules;
pub mod paths;
pub mod permissions;
pub mod reads;
pub mod roles;
pub mod services;
pub mod spec;
pub mod store;
pub mod stream_providers;
pub mod table_providers;
pub mod tables;
pub mod view_runtime;
pub mod webhooks;

pub use action::ModuleAction;
pub use bounds::{
    DEFAULT_CALL_TIMEOUT, DEFAULT_MODULE_JS_SLICE, DEFAULT_MODULE_MAX_HEAP, DEFAULT_MODULE_WORKERS,
};
pub use bundled::{BUNDLED_IN_CHECKOUT, BundledModule, BundledModules};
pub use csp::{CspSuggestion, SUGGESTIBLE_DIRECTIVES};
#[cfg(feature = "deno-host")]
pub use deno::{DenoModuleHost, PoolBounds};
pub use frameworks::ModuleFrameworks;
pub use functions::ModuleFunctions;
pub use host::{
    ActionManifest, CallHosts, FrameworkManifest, FunctionArg, FunctionManifest,
    ModelProviderManifest, ModuleHost, ModuleManifest, OAuthAsks, ServiceAsks, ServiceManifest,
    TableProviderManifest, UnsupportedEntity, WebhookManifest, prime_v8,
};
pub use install::{
    InstalledPackage, Installer, MIN_NPM_VERSION, have_node, have_npm, npm_too_old, npm_version,
};
pub use model_providers::ModuleModelProviders;
pub use module::{
    MODULE_LANGUAGES, MODULE_SOURCES, Module, ModuleId, ModuleLanguage, ModuleSource,
};
pub use modules::{LoadedModule, ModuleIssue, ModuleSet, redacted_configuration, unsupported_json};
pub use paths::default_modules_root;
pub use permissions::{
    ANY_HOST, ModulePermissions, PERM_ENV, PERM_NET, PERM_READ, PERM_ROLES, PERM_WRITE,
};
pub use roles::ModuleRoles;
pub use services::{Backoff, SERVICE_EVENTS, ServiceState, ServiceStatus, ServiceSupervisor};
pub use spec::config_fields_to_form_fields;
pub use store::{
    COL_LANGUAGE, COL_PERMISSIONS, MODULES_TABLE, bootstrap_modules, delete_module, list_modules,
    load_module, load_module_by_name, require_module, save_module,
};
pub use stream_providers::ModuleStreamProviders;
pub use table_providers::ModuleTableProviders;
pub use tables::{FieldDecl, MODULE_TABLE_KEY, SyncReport, TableDecl};
pub use view_runtime::{BUILTIN_VIEW_RUNTIME, ModuleViewRuntime};
pub use webhooks::{
    HOOKS_PREFIX, WEBHOOK_MAX_BODY, WebhookRequest, WebhookResponse, hook_path, hook_segment,
    run_webhook, webhook_module,
};
