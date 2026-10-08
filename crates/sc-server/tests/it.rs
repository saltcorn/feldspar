//! Every integration test in this crate, in one binary.
//!
//! Each file below is still an ordinary test file — it is pulled in as a module
//! rather than compiled as its own target. The workspace statically links V8 into
//! every test binary, so a target per file cost ~400 MB of disk and a link each;
//! CI ran out of disk on the link (`ld terminated with signal 7`) before it ran
//! out of patience. Files stay where they are, so paths relative to a test file
//! (fixtures, `include_str!`, `#[path]`) are unaffected.
//!
//! Add a new test file and it is picked up here — the list is the whole wiring.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "admin_api.rs"]
mod admin_api;
#[path = "admin_applications_api.rs"]
mod admin_applications_api;
#[path = "admin_graphql_explorer.rs"]
mod admin_graphql_explorer;
#[path = "admin_spa_typecheck.rs"]
mod admin_spa_typecheck;
#[path = "admin_theme.rs"]
mod admin_theme;
#[path = "admin_users_api.rs"]
mod admin_users_api;
#[path = "admin_workflow_editor.rs"]
mod admin_workflow_editor;
#[path = "agent_admin_api.rs"]
mod agent_admin_api;
#[path = "agent_chat.rs"]
mod agent_chat;
#[path = "analytics_api.rs"]
mod analytics_api;
#[path = "analytics_done.rs"]
mod analytics_done;
#[path = "analytics_plots.rs"]
mod analytics_plots;
#[path = "analytics_route.rs"]
mod analytics_route;
#[path = "analytics_spa_typecheck.rs"]
mod analytics_spa_typecheck;
#[path = "api_token_admin_api.rs"]
mod api_token_admin_api;
#[path = "app_builder_agent.rs"]
mod app_builder_agent;
#[path = "app_file_access.rs"]
mod app_file_access;
#[path = "app_i18n.rs"]
mod app_i18n;
#[path = "app_invite.rs"]
mod app_invite;
#[path = "app_serving.rs"]
mod app_serving;
#[path = "app_signup.rs"]
mod app_signup;
#[path = "app_static_dirs.rs"]
mod app_static_dirs;
#[path = "app_streams.rs"]
mod app_streams;
#[path = "app_trigger_api.rs"]
mod app_trigger_api;
#[path = "backup_api.rs"]
mod backup_api;
#[path = "builder_route.rs"]
mod builder_route;
#[path = "call_api.rs"]
mod call_api;
#[path = "clear_all_api.rs"]
mod clear_all_api;
#[path = "code_body_tables.rs"]
mod code_body_tables;
#[path = "concurrent_code_bodies.rs"]
mod concurrent_code_bodies;
#[path = "constraint_api.rs"]
mod constraint_api;
#[path = "db_connection_admin_api.rs"]
mod db_connection_admin_api;
#[path = "field_api.rs"]
mod field_api;
#[path = "file_manager.rs"]
mod file_manager;
#[path = "file_operations_api.rs"]
mod file_operations_api;
#[path = "file_store_admin_api.rs"]
mod file_store_admin_api;
#[path = "fit_model_action.rs"]
mod fit_model_action;
#[path = "generated_client_refresh.rs"]
mod generated_client_refresh;
#[path = "geo_import.rs"]
mod geo_import;
#[path = "geometry.rs"]
mod geometry;
#[path = "graphql_serving.rs"]
mod graphql_serving;
#[path = "ide_language_server.rs"]
mod ide_language_server;
#[path = "ide_route.rs"]
mod ide_route;
#[path = "ide_typecheck.rs"]
mod ide_typecheck;
#[path = "live_mounting.rs"]
mod live_mounting;
#[path = "llm_provider_admin_api.rs"]
mod llm_provider_admin_api;
#[path = "locale_negotiation.rs"]
mod locale_negotiation;
#[path = "mcp_server.rs"]
mod mcp_server;
#[path = "metadata_tables_api.rs"]
mod metadata_tables_api;
#[path = "model_admin_api.rs"]
mod model_admin_api;
#[path = "model_dataset.rs"]
mod model_dataset;
#[path = "model_editor_api.rs"]
mod model_editor_api;
#[path = "model_fit_job.rs"]
mod model_fit_job;
#[path = "model_formulas.rs"]
mod model_formulas;
#[path = "model_handle.rs"]
mod model_handle;
#[path = "models_without_actions.rs"]
mod models_without_actions;
#[path = "modules_api.rs"]
mod modules_api;
#[path = "named_datasets.rs"]
mod named_datasets;
#[path = "other_events.rs"]
mod other_events;
#[path = "ownership_enforcement.rs"]
mod ownership_enforcement;
#[path = "ownership_settings_api.rs"]
mod ownership_settings_api;
#[path = "posterior_api.rs"]
mod posterior_api;
#[path = "primary_key_api.rs"]
mod primary_key_api;
#[path = "provided_tables_api.rs"]
mod provided_tables_api;
#[path = "python_trigger.rs"]
mod python_trigger;
#[path = "rls_enforcement.rs"]
mod rls_enforcement;
#[path = "router.rs"]
mod router;
/// The process-wide registries `ModuleServices` installs into on boot and on
/// every module change — the view runtime, the view pattern registry, a
/// plugin's headers and public files, the frameworks — held against the tests
/// in this binary that boot one.
///
/// A server is one process, so in production these are globals for good
/// reason; here many servers share one process. A test that boots the modules
/// takes the lock **for reading**, and they run side by side as before. A test
/// that needs the registries to stay what *its* server installed — an installed
/// plugin's view patterns (TODO "Saltcorn UI" Phase 11) — takes it **for
/// writing**, so no other server's boot replaces them mid-test.
pub(crate) fn module_registries() -> &'static tokio::sync::RwLock<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::RwLock<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::RwLock::new(()))
}

#[path = "app_none_framework.rs"]
mod app_none_framework;
#[path = "app_preview.rs"]
mod app_preview;
#[path = "app_public_endpoints.rs"]
mod app_public_endpoints;
#[path = "saltcorn_ui_admin_api.rs"]
mod saltcorn_ui_admin_api;
#[path = "saltcorn_ui_configure.rs"]
mod saltcorn_ui_configure;
#[path = "saltcorn_ui_mount.rs"]
mod saltcorn_ui_mount;
#[path = "saltcorn_ui_plugins.rs"]
mod saltcorn_ui_plugins;
#[path = "saltcorn_ui_render.rs"]
mod saltcorn_ui_render;
#[path = "schema_edit_api.rs"]
mod schema_edit_api;
#[path = "settings_admin_api.rs"]
mod settings_admin_api;
#[path = "stan_models.rs"]
mod stan_models;
#[path = "stream_triggers.rs"]
mod stream_triggers;
#[path = "streams_admin_api.rs"]
mod streams_admin_api;
#[path = "table_access_enforcement.rs"]
mod table_access_enforcement;
#[path = "table_csv_api.rs"]
mod table_csv_api;
#[path = "table_csv_import.rs"]
mod table_csv_import;
#[path = "table_settings_api.rs"]
mod table_settings_api;
#[path = "table_triggers.rs"]
mod table_triggers;
#[path = "tls_live_domains.rs"]
mod tls_live_domains;
#[path = "tls_serving.rs"]
mod tls_serving;
#[path = "trigger_admin_api.rs"]
mod trigger_admin_api;
#[path = "tutorial_workflows.rs"]
mod tutorial_workflows;
#[path = "view_app.rs"]
mod view_app;
#[path = "workflow_admin_api.rs"]
mod workflow_admin_api;
