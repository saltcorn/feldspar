//! **What a module is told about an application's life** (TODO-app-constructor
//! AC12.5): `on_application_deleted(app, ctx)`, so a module deletes what it
//! keeps for an application when the application is deleted.
//!
//! ```js
//! on_application_deleted: async (app, ctx) => {
//!   await ctx.db.constructions.where({ application: app.id }).delete();
//!   await ctx.agents.remove(`constructor-${app.subdomain}`);
//! },
//! ```
//!
//! The hook runs **as the module** over its own tables, as a webhook does —
//! the application is gone, and nobody's request is waiting on what the module
//! keeps for it — with `ctx.agents` and `ctx.apps` as a service has them. Each
//! module is told in turn; one that fails is reported and does not stop the
//! others, nor the deletion, which has already happened.

use sc_action::TriggerDispatcher;
use sc_api::code_host::{TableHost, TriggerRunHost, schema_snapshot};
use sc_catalog::{AdminPrincipal, Catalog};
use sc_error::Result;
use sc_expr::{CodeHosts, TriggerHost};
use serde_json::{Value as Json, json};

use crate::files::{ModuleFiles, file_areas};
use crate::host::{AskSurface, Asks, CallHosts, ModuleHost};
use crate::module::ModuleLanguage;
use crate::modules::{LoadedModule, ModuleSet};
use crate::server_asks::ServerAsks;
use crate::services::ScopedTables;

/// What stands behind a module hook's admin authority: an installed module's
/// hook, which an administrator installed, called by the server when an
/// administrator deleted an application, its `db` confined to the module's own
/// tables. Listed in the system-authority inventory.
pub const MODULE_HOOK: &str = "a module's application-deleted hook (sc-module lifecycle.rs)";

/// The modules of `set` with an `on_application_deleted` hook.
pub fn application_deleted_modules(set: &ModuleSet) -> Vec<&LoadedModule> {
    set.modules()
        .iter()
        .filter(|m| m.module.language == ModuleLanguage::JavaScript)
        .filter(|m| m.manifest.as_ref().is_some_and(|mf| mf.application_deleted))
        .collect()
}

/// Tell `loaded` the application `app` (as it was) was deleted.
pub async fn application_deleted(
    catalog: &Catalog,
    host: &ModuleHost,
    dispatcher: Option<&TriggerDispatcher>,
    loaded: &LoadedModule,
    app: Json,
    server: Option<&dyn ServerAsks>,
) -> Result<()> {
    let module = loaded.module.name.as_str();
    let tables = crate::services::TableScope::of(loaded)
        .with_trigger_reads(catalog, loaded)
        .await;
    let areas = file_areas(&loaded.table_declarations());
    let admin = AdminPrincipal::vouched(
        Some(json!({ "module": module, "hook": "application_deleted" })),
        MODULE_HOOK,
    );
    let scoped = ScopedTables {
        inner: TableHost::new(catalog).for_admin(&admin),
        catalog,
        module,
        tables: &tables,
        kind: "hook",
    };
    let triggers = dispatcher.map(|d| TriggerRunHost::new(d, catalog).for_admin(&admin));
    let files = ModuleFiles::new(catalog, module, &areas);
    let schema = schema_snapshot(catalog).ok();
    let permissions = &loaded.module.permissions;
    let agents = server.map(|s| s.agents(module, permissions.agents, None));
    let apps = server.map(|s| s.apps(module, permissions.apps, None));
    let mut asks = Asks::default();
    if let Some(agents) = &agents {
        asks = asks.with(AskSurface::Agents, agents.as_ref());
    }
    if let Some(apps) = &apps {
        asks = asks.with(AskSurface::Apps, apps.as_ref());
    }
    let call = CallHosts {
        hosts: CodeHosts {
            host: Some(&scoped),
            triggers: triggers.as_ref().map(|t| t as &dyn TriggerHost),
            ..CodeHosts::default()
        },
        schema: schema.as_deref(),
        views: None,
        module_files: Some(&files),
        asks,
    };
    host.application_deleted(module, app, call).await?;
    Ok(())
}
