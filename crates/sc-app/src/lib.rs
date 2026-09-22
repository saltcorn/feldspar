//! Application model, Framework trait, routing (layer 8; design §13.2–§13.3).
//!
//! An [`Application`] is v2's unit of multi-tenancy: multiple apps share one data
//! layer, each seeing only its declared subset of tables and file stores, served
//! on its own subdomain by one primary [`Framework`] with a strict [`CspPolicy`]
//! by default. A [`Framework`] owns an app's UI; [`CodeFramework`] is the MVP
//! implementation, serving a pre-built [`AssetBundle`] of static files (a bundled
//! React/Svelte/… SPA) with an SPA fallback so client-routed deep links resolve.
//!
//! A code framework's source lives in a git repository inside one of the app's
//! file stores and has a **build step**: [`build_code_framework`] invokes the
//! bundler over that source and hands back a [`CodeFramework`] serving the built
//! bundle.
//!
//! Two code frameworks are registered (§13.3). `code` is the generic one — any
//! bundler, any layout, stated as five settings. [`react`](crate::react) is the
//! opinionated one: it asks for the file store and a project directory and
//! *derives* the rest, and (from §2.3) the server scaffolds the project itself.
//! It is not a second serving implementation — a built React app is a static
//! bundle with an SPA fallback, so it mounts as a [`CodeFramework`] under its own
//! name, and [`app_source_from_config`] resolves both to the same [`AppSource`].
//!
//! An [`Application`] is pure data; [`app_providers`]/[`app_endpoints`] are the
//! wiring that resolves it into running API providers and the single endpoint set
//! they project. [`app_client`] generates the app's typed TypeScript client from
//! that set — the same generator the admin SPA uses — and [`build_application`]
//! emits it into the app's source tree before invoking the bundler.
//!
//! Applications are **created in the admin UI, not in Rust** (§13.2), so an app
//! is defined by its `_fd_applications` row and nothing else — there is nothing
//! to introspect one from, which is why this is the one stored-metadata table
//! the MVP needs. [`bootstrap`] creates the table (idempotently, on any database
//! including one that has never seen Saltcorn) and [`save_application`] /
//! [`load_application`] / [`list_applications`] / [`delete_application`] are the
//! row ⇄ [`Application`] path. Saving is not building or mounting: an app that is
//! saved but unbuilt is a normal state.
//!
//! An application is also created with the **agent that builds it**, and which
//! agent that is belongs to its framework: [`framework_builder_agent`] is the
//! declaration, beside the framework's settings and its default CSP. Both code
//! frameworks declare a coding agent over the source tree they build from; the
//! record itself is created by the server, which is the layer that knows agents
//! exist.

mod api;
mod application;
mod applications;
mod build;
mod builder_agent;
// The frameworks a module declares (§13.3, §15.1): the same registry answers,
// written down as data instead of compiled.
mod declared;
mod diagnostics;
// Frameworks written in Rust above this crate (Saltcorn UI): named constructors.
mod factory;
mod framework;
// An application's own catalogue: the `CatalogStore` seam and `_fd_translations`
// (§16.1, D4).
pub mod i18n;
// The application third of the administrative tool surface (§13.6), and the one
// constructor of the whole nine-tool set.
pub mod mcp;
mod react;
mod scaffold;
mod skill;
mod store;
mod streams;

pub use api::{
    ApiProviderInfo, AppGraphql, api_provider_config_spec, app_client, app_client_with,
    app_endpoints, app_endpoints_with, app_graphql, app_providers, app_providers_with,
    app_schema_sql, app_tables, app_triggers, registered_api_provider_info, select_api,
    serves_custom_queries, validate_api_config, validate_api_mounts, validate_static_dirs,
};
pub use application::{
    ApiConfig, AppId, Application, CspPolicy, FRAME_ANCESTORS, FrameworkRef, StaticDir, StreamRef,
    TriggerRef, allow_admin_framing,
};
pub use applications::{
    APPLICATIONS_TABLE, COL_APIS, COL_ATTRIBUTES, COL_CSP, COL_DESCRIPTION, COL_EXTRA_FRAMEWORKS,
    COL_FILE_STORES, COL_FRAMEWORK, COL_ID, COL_NAME, COL_STATIC_DIRS, COL_STREAMS, COL_SUBDOMAIN,
    COL_TABLES, COL_TRIGGERS, bootstrap,
};
pub use build::{
    AppSource, BuildReport, app_source_from_config, app_source_in, build_app, build_application,
    build_code_framework, emit_app_client, emit_client, load_app_bundle, run_build,
};
pub use builder_agent::{
    BuilderAgentSpec, BuilderTrait, EDIT_FORMAT_AUTO, TRAIT_CFG_APPLICATION, TRAIT_CFG_CHECKS,
    TRAIT_CFG_EDIT_FORMAT, TRAIT_CFG_MAY_CHECK, TRAIT_CFG_MAY_EDIT, TRAIT_CFG_MAY_RUN_SCRIPTS,
    TRAIT_CFG_MAY_USE_SHELL, TRAIT_CFG_MAY_VIEW_APP, TRAIT_CFG_PREVIEW_RELOAD,
    TRAIT_CFG_PREVIEW_URL, TRAIT_CFG_ROOT, TRAIT_CFG_STORE, TRAIT_CFG_WORKFLOW, TRAIT_CODING,
    TRAIT_PREVIEW_PANE, WORKFLOW_PLANNED, builder_agent_in, builder_agent_name,
    framework_builder_agent, preview_pane_url,
};
pub use declared::{
    BuildTemplate, DeclaredFile, FilePhase, FrameworkDecl, FrameworkHost, FrameworkSet,
    PathTemplate, clean_path, declared_framework, install_frameworks, installed_frameworks,
};
pub use diagnostics::{Diagnostic, build_diagnostics, parse_diagnostics};
pub use factory::{
    FrameworkFactory, MountContext, framework_factories, framework_factory,
    install_framework_factory,
};
pub use framework::{
    AppRequest, AppResponse, Asset, AssetBundle, BuildSpec, CFG_CLIENT, CFG_COMMAND, CFG_OUTPUT,
    CFG_SOURCE, CFG_STORE, CODE_FRAMEWORK, CodeFramework, Framework, FrameworkInfo, InstallSpec,
    Method, RequestBody, asset_content_type, code_config_spec, config_spec_in, default_csp_in,
    framework_config_spec, framework_default_csp, framework_info_in, framework_serves_ui,
    registered_framework_info, registered_frameworks, serves_ui_in, validate_config_in,
    validate_config_structure_in, validate_framework_config, validate_framework_config_structure,
};
pub use i18n::{
    ATTR_DEFAULT_LOCALE, ATTR_LOCALES, CatalogStore, FileCatalogStore, I18N_SEGMENT, LOCALES_DIR,
    RowCatalogStore, TRANSLATIONS_TABLE, app_catalog_store, app_default_locale, app_is_translated,
    app_locales, bootstrap_translations, delete_application_translations, i18n_catalog_path,
    i18n_catalog_path_template, i18n_locale_in_path, set_app_locales,
};
pub use react::{
    CFG_PROJECT, REACT_BUILD_ARGS, REACT_BUILD_COMMAND, REACT_CLIENT_FILE, REACT_FRAMEWORK,
    REACT_OUTPUT_SUBDIR, REACT_RUNTIME_SUBDIR, check_project_name, project_description,
    project_path, react_build_spec, react_client_path, react_config_spec, react_csp,
    react_runtime_dir, valid_project_name,
};
pub use scaffold::{
    ClientUpdate, GeneratedFile, ScaffoldReport, emit_app_runtime, has_generated_runtime,
    require_api_provider, require_scaffoldable, require_scaffoldable_in, scaffold_app,
    update_app_client,
};
pub use skill::{SKILL_FILE, generate_skill};
pub use store::{
    applications_using_file_store, delete_application, list_applications, load_application,
    load_application_by_subdomain, save_application,
};
pub use streams::{
    ExposedStream, app_streams, element_value_schema, install_stream_registry, stream_exports,
    stream_in_path, stream_registry, stream_socket_path,
};
