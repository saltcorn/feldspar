//! The concrete admin-API handlers (technical design §13.1, §7).
//!
//! [`admin_handlers`] builds the [`HandlerRegistry`] that the admin
//! [`EndpointSet`](sc_api::admin_endpoints) dispatches against: one entry per
//! endpoint `name`. The handlers stay free of HTTP plumbing: they read a
//! [`HandlerCtx`] and return a [`HandlerResponse`], deferring cookies/sessions to
//! the dispatcher via [`SessionAction`](crate::SessionAction).
//!
//! **One module per domain**, each with a
//! `register(reg, state)` and the helpers only it uses, mirroring the module of
//! the same name under `sc-api`'s `admin/`, which declares the endpoints. What
//! more than one domain uses is here, and the entity codecs are
//! `sc_backup::codec`. A domain reaches the catalog and the mount registry
//! through its [`HandlerState`]; a handler written as a named function is
//! registered through [`typed`](crate::handler::typed), which reads its body
//! into a type and is how [`AdminHost`](sc_catalog::AdminHost) calls the same
//! function directly. Handlers move to it as they are touched.
//!
//! Data crosses the wire as plain JSON. The row endpoints are thin wrappers over
//! [`sc_access::rows`], the shared row-CRUD layer an application's REST API runs
//! too — the admin API is not a privileged special case, it is the same
//! machinery (design §13.1). A row is addressed by its single-column primary
//! key; `createTable` invents none (GOALS), so a new table has no key until a
//! field says it is one, and until then the row endpoints that address a single
//! row refuse — which is what the field list's red banner is warning about.

use sc_catalog::Authority;
use sc_types::Role;
use sc_types::{ArgsExt, Attrs};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use sc_access::rows::{self, require_object};
use sc_action::{
    EventKind, Trigger, TriggerBody, TriggerId, delete_trigger, list_triggers, load_trigger,
    save_trigger,
};
use sc_api::auth::{credentials, user_row_json, user_summary_json};
use sc_api::csv as csv_rows;
use sc_api::history::HistoryReader;
use sc_api::schema_edit;
use sc_api::undo::Reverser;
use sc_api::{ApiRequest, GRAPHQL_PROVIDER, Method as ApiMethod};
use sc_app::{
    AppId, Application, app_source_from_config, applications_using_file_store, builder_agent_name,
    delete_application, framework_builder_agent, framework_config_spec, list_applications,
    load_application, registered_api_provider_info, registered_framework_info,
    require_scaffoldable, save_application, scaffold_app, update_app_client,
};
use sc_auth::{
    COL_EMAIL, NewApiToken, NewUser, RoleDef, USERS_TABLE, User, UserUpdate, any_user_exists,
    authenticate_admin, create_first_user, create_user_with, delete_role, delete_user,
    list_api_tokens, list_roles, load_user, mint_api_token, random_password, revoke_api_token,
    save_role, set_user_disabled, set_user_password, update_user,
};
use sc_catalog::{
    Catalog, ConstraintKind, DataField, DataFieldKind, DbConnectionDef, DbConnectionId,
    FIELD_META_TABLE, FieldMeta, SchemaProjection, Table, TableConstraint,
    check_db_connection_saveable, check_file_store_saveable, connect_db_connection,
    connect_file_store_def, delete_db_connection, delete_file_store, file_kind_config_spec,
    key_kind_config_spec, list_db_connections, list_field_meta_for_table, list_file_stores,
    load_db_connection, load_db_connection_by_name, load_file_store, load_file_store_by_name,
    orphan_table_meta, resolve_options, save_db_connection, save_file_store,
};
use sc_catalog::{
    NEW_LOCAL_FILE_STORE, choosable_file_stores, file_store_settings, unique_file_store_name,
};
use sc_email::Mailer;
use sc_error::{Context as _, Error, Result};
use sc_files::{
    CheckedStore, Entry, FileMeta, FileStoreDef, FileStoreDefId, VisibleEntry, backend_config_spec,
    backend_operations, registered_backends, run_backend_operation,
};
use sc_i18n::Locale;
use sc_i18n::t;
use sc_llm::{
    LlmModelDef, LlmModelDefId, LlmProviderDef, LlmProviderDefId, LlmRequest, connect_model,
    delete_llm_model, delete_llm_provider, fetch_host_models, list_llm_models, list_llm_providers,
    load_llm_model, load_llm_provider, model_config_spec, provider_config_spec, save_llm_model,
    save_llm_provider,
};
use sc_module::load_module;
use sc_query::{Expr, OrderBy, Projection, Select, Source, Statement, Value};
use sc_types::{
    FormField, Operation, OperationScope, SECRET_SENTINEL, registered_rich_types,
    rich_type_config_spec,
};
use serde_json::{Map, Value as Json, json};

// The entity codecs (`*_json` / `*_from_body`) and the body-reading helpers
// they are built from: one set for the admin API and a backup.
use sc_backup::codec::*;

use crate::apps::{AppMounts, build_and_mount};
use crate::handler::{Admin, HandlerCtx, HandlerRegistry, HandlerResponse, typed};

mod agents;
pub(crate) mod applications;
mod auth;
mod backup;
mod code_libraries;
mod connections;
mod fields;
pub(crate) mod file_stores;
mod files;
mod llm;
mod models;
mod modules;
mod settings;
pub(crate) mod streams;
mod table_rows;
mod tables;
pub(crate) mod translations;
mod triggers;
mod users;
mod views;

/// What a handler works on: the catalog and the live mount registry, which
/// reaches the trigger dispatcher, the agents, models, modules and streams.
///
/// Every domain's `register` is handed one, and a typed handler
/// ([`typed`](crate::handler::typed)) is handed its own clone.
#[derive(Clone)]
pub(crate) struct HandlerState {
    /// The server's catalog.
    pub(crate) catalog: Arc<Catalog>,
    /// The server's mount registry, which the application handlers mount and
    /// unmount through and the services are reached from.
    pub(crate) apps: Arc<AppMounts>,
}

/// Build the registry of admin handlers over a shared [`Catalog`] and the live
/// [`AppMounts`] registry.
///
/// The names match the [`Endpoint`](sc_api::Endpoint) `name`s in
/// [`admin_endpoints`](sc_api::admin_endpoints); the router resolves each
/// endpoint's [`HandlerRef::Named`](sc_api::HandlerRef) here at dispatch time.
///
/// `apps` is the **same** live handle the router resolves requests against, so
/// the application `build`/`delete` handlers mount and unmount apps that start
/// serving (or stop) with no restart (§13.2). For an admin-only server pass an
/// [`AppMounts`] with the catalog attached ([`AppMounts::new`]); the application
/// CRUD handlers still work, and a build simply mounts an app no subdomain routes
/// to until a `--base-domain` is configured.
pub fn admin_handlers(catalog: Arc<Catalog>, apps: Arc<AppMounts>) -> HandlerRegistry {
    admin_handlers_with(
        catalog,
        apps,
        Arc::new(crate::public_datasets::HttpFetch::new()),
    )
}

/// [`admin_handlers`], with the public datasets' files downloaded by `fetch`
/// rather than over HTTPS — what a test serves fixture files through.
pub fn admin_handlers_with(
    catalog: Arc<Catalog>,
    apps: Arc<AppMounts>,
    fetch: Arc<dyn sc_api::public_datasets::Fetch>,
) -> HandlerRegistry {
    // A schema change re-projects the API providers of every mounted app
    // exposing that table, live. A request or an agent (§11.3) can change a
    // schema, and `sc-api`'s schema editor — where the change is made — cannot
    // name this crate. So the catalog carries the seam and the observer is
    // installed **here**, at the one place a server has both handles: a caller
    // that assembles the admin API gets live re-projection by construction
    // rather than by remembering to ask for it.
    catalog.set_schema_observer(Arc::clone(&apps) as Arc<dyn sc_catalog::SchemaObserver>);
    // And what follows a table rename into the views, triggers, applications
    // and other definitions that name the table (`table_rename`), for the same
    // reason: the editor that renames it cannot name the crates that own them.
    catalog.set_table_rename_hook(Arc::clone(&apps) as Arc<dyn sc_catalog::TableRenameHook>);
    // And the same for the trigger set, installed on the dispatcher rather than
    // the catalog because that is what holds it: a saved, renamed or deleted
    // trigger re-projects every app exposing one, whether the save came from
    // these handlers or from an agent's `admin_copilot` (§11.3). The handlers do
    // not re-project themselves, so nothing fires twice.
    if let Ok(dispatcher) = triggers_of(&apps) {
        dispatcher.set_observer(Arc::clone(&apps) as Arc<dyn sc_action::TriggerObserver>);
    }

    let mut reg = HandlerRegistry::new();
    let state = HandlerState {
        catalog: catalog.clone(),
        apps: apps.clone(),
    };
    auth::register(&mut reg, &state);
    tables::register(&mut reg, &state);
    fields::register(&mut reg, &state);
    table_rows::register(&mut reg, &state);
    connections::register(&mut reg, &state);
    file_stores::register(&mut reg, &state);
    files::register(&mut reg, &state);
    llm::register(&mut reg, &state);
    modules::register(&mut reg, &state);
    agents::register(&mut reg, &state);
    code_libraries::register(&mut reg, &state);
    triggers::register(&mut reg, &state);
    models::register(&mut reg, &state);
    streams::register(&mut reg, &state);
    settings::register(&mut reg, &state);
    backup::register(&mut reg, &state);
    applications::register(&mut reg, &state);
    views::register(&mut reg, &state);
    translations::register(&mut reg, &state);
    users::register(&mut reg, &state);

    // --- the Analytics UI -----------------------------
    crate::analytics::register(&mut reg, catalog.clone());
    crate::public_datasets::register(&mut reg, catalog.clone(), fetch);
    // --- the home page's entities list ----------------------------------------
    crate::entities::register(&mut reg, catalog.clone(), apps.clone());
    // --- remote MCP servers an agent calls (TODO-agent-skills AS4) -----------
    crate::mcp_servers::register(&mut reg, catalog.clone());
    // --- what agents remember (TODO-agent-skills AS8.3) ----------------------
    crate::agent_memory::register(&mut reg, catalog.clone());
    // --- checklists ----------------------------------
    crate::checklists::register(&mut reg, catalog.clone(), apps.clone());
    // --- the App Constructor (TODO-app-constructor) ---------------------------
    crate::constructions::register(&mut reg, catalog.clone(), apps.clone());

    reg
}

// --- request/response shaping --------------------------------------------------
//
// The helpers several domains share. The entity codecs themselves — every
// `*_json` / `*_from_body` a backup also writes and reads — are
// `sc_backup::codec`, imported above, so the admin API and a backup have one
// wire shape between them.

// --- applications: wire shaping ------------------------------------------------

/// [`application_json`] as an admin is shown it: the framework's
/// [`secret`](sc_types::FormField::secret) settings (a keystore password)
/// replaced by the sentinel, which a save hands back and
/// `merge_framework_secrets` restores. The unredacted form is what a backup
/// keeps.
pub(crate) fn application_api_json(app: &Application) -> Json {
    let mut out = application_json(app);
    if let Ok(spec) = framework_config_spec(&app.framework.name) {
        out["framework"]["config"] =
            Json::Object(sc_types::redact_attrs(&spec, &app.framework.config));
    }
    out
}

/// A submitted application's framework settings with every secret the form
/// handed back unchanged (the sentinel) replaced by what `stored` holds — so
/// saving an application's name does not save the mask over its password.
fn merge_framework_secrets(stored: &Application, submitted: &mut Application) {
    if stored.framework.name != submitted.framework.name {
        return;
    }
    if let Ok(spec) = framework_config_spec(&submitted.framework.name) {
        submitted.framework.config =
            sc_types::merge_secrets(&spec, &stored.framework.config, &submitted.framework.config);
    }
}

/// An LLM provider as the API returns it (matching `llm_provider_schema`), with
/// its backend's `secret` settings replaced by the sentinel.
///
/// **The redaction is here and nowhere else.** Every response that carries a
/// provider goes through this function, so an endpoint added later cannot
/// accidentally return a key: it would have to build the JSON by hand to do so.
///
/// `default` is the installation's default provider's id
/// ([`default_provider_id`]), passed in so a listing asks once.
fn llm_provider_json(
    catalog: &Catalog,
    def: &LlmProviderDef,
    default: Option<LlmProviderDefId>,
) -> Json {
    json!({
        "id": def.id.0,
        "name": def.name,
        "description": def.description,
        "backend": def.backend,
        "config": Json::Object(redacted_provider_config(&def.backend, &def.config)),
        "from_config_file": sc_llm::is_host_llm_provider(catalog, def.id),
        "is_default": default == Some(def.id),
    })
}

/// The id of the installation's default LLM provider, if there is a provider.
async fn default_provider_id(catalog: &Catalog) -> Result<Option<LlmProviderDefId>> {
    Ok(sc_llm::default_llm_provider(catalog)
        .await?
        .map(|def| def.id))
}

/// A provider config with its backend's [`secret`](FormField::secret) settings
/// replaced by the sentinel.
///
/// An unknown backend redacts nothing rather than failing, for the same reason
/// `redacted_config` does for stores: a provider whose backend is no longer
/// registered must stay listable so the admin can repoint or delete it, and no
/// spec means no field was declared secret in the first place.
fn redacted_provider_config(backend: &str, config: &sc_types::Attrs) -> sc_types::Attrs {
    match provider_config_spec(backend) {
        Ok(spec) => sc_types::redact_attrs(&spec, config),
        Err(_) => config.clone(),
    }
}

/// What `listModules` answers: the installed modules, the bundled catalog, and
/// whether npm, python and pip are here to install more.
///
/// Shared by the admin handler and by the agents' [`sc_agent::ModuleManager`]
/// (and so the MCP tool, which is the handler), so the three read one answer.
pub(crate) async fn list_modules_json(services: &crate::ModuleServices, locale: &Locale) -> Json {
    let set = services.modules();
    let modules: Vec<Json> = set
        .modules()
        .iter()
        .map(|m| module_json(m, &services.service_statuses(&m.module.name), locale))
        .collect();
    // Installed is by **package name**, which is what the row is
    // keyed by: a bundled module installed from a checkout, from a
    // tarball, or from the registry it was also published to is one
    // module, and the card should say so however it got there.
    let bundled: Vec<Json> = services
        .bundled()
        .modules()
        .iter()
        .map(|entry| bundled_json(entry, set.get(&entry.name).is_some()))
        .collect();
    let interpreter = services.python().env().interpreter();
    // Asked once and used twice: whether there is an npm at all,
    // and whether it is one that can install a module. An npm too
    // old for the modules root's overrides fails every install with
    // a semver error, so the tab says so before the admin picks a
    // package rather than after (see `sc_module`'s installer).
    let npm_version = sc_module::npm_version().await;
    let npm_too_old = match &npm_version {
        Some(version) if sc_module::npm_too_old(version) => json!({
            "version": version,
            "minimum": sc_module::MIN_NPM_VERSION,
        }),
        _ => Json::Null,
    };
    json!({
        "modules": modules,
        "root": services.installer().root().display().to_string(),
        // Asked here rather than assumed, because a server with no
        // Node toolchain can do everything else and nothing on this
        // tab — and the tab should say so before the admin types a
        // package name.
        "npm": npm_version.is_some(),
        // Null when npm can install a module, and the two versions
        // the sentence needs when it cannot.
        "npm_too_old": npm_too_old,
        "node": sc_module::have_node().await,
        // And the other language's, asked the same way and for the
        // same reason (§8): a server may have one toolchain and not
        // the other, and which of the two install forms can work is
        // not something an admin should learn by using one.
        "python": sc_python::have_python(&interpreter).await,
        "pip": sc_python::have_pip(&interpreter).await,
        // Where pip installs, beside where npm does. `None` only on
        // a machine with no data directory to default to.
        "python_dir": match services.python().env().directory() {
            Some(dir) => Json::from(dir.display().to_string()),
            None => Json::Null,
        },
        "bundled": bundled,
    })
}

/// What `installModule` does with its body — `source`, `location` and an
/// optional `language` — and the loaded module it answers with: install the
/// package, write or update its row, reload the module set.
///
/// Shared by the admin handler (and so its MCP tool) and the agents'
/// [`sc_agent::ModuleManager`], so an install from any of them is the same act.
pub(crate) async fn install_module_json(
    services: &crate::ModuleServices,
    body: &Json,
    locale: &Locale,
) -> Result<Json> {
    let obj = require_object(body)?;
    let source = sc_module::ModuleSource::parse(obj.non_empty_str_field("source")?)?;
    let location = obj.non_empty_str_field("location")?.trim().to_owned();
    // Absent is `javascript`: the language nearly every module is
    // written in, and the one every existing caller means.
    let language = match obj.get("language").filter(|v| !v.is_null()) {
        Some(value) => sc_module::ModuleLanguage::parse(
            value
                .as_str()
                .ok_or_else(|| Error::invalid("field `language` must be a string"))?,
        )?,
        None => sc_module::ModuleLanguage::JavaScript,
    };
    // A **bundled** module's language and permissions are the catalog's,
    // not the request's — see `ModuleServices::install_module`, which is
    // also what `feldspar module install` runs.
    let module = services
        .install_module(language, source, &location, |_| Ok(()))
        .await?;
    let set = services.modules();
    let loaded = set
        .get(&module.name)
        .ok_or_else(|| Error::msg("the installed module is not in the loaded set"))?;
    Ok(module_json(
        loaded,
        &services.service_statuses(&module.name),
        locale,
    ))
}

/// One loaded module as JSON (matching `module_schema`): the row, what the
/// package supplies, and everything wrong with it.
///
/// The actions' settings are translated here rather than carried from the
/// registry because they are the same translation — v1's `configFields` through
/// [`sc_module::config_fields_to_form_fields`] — and a module that failed to
/// register an action (a name clash) still has settings the tab should show
/// beside the reason it is not available.
fn module_json(
    loaded: &sc_module::LoadedModule,
    statuses: &[sc_module::ServiceStatus],
    locale: &Locale,
) -> Json {
    let module = &loaded.module;
    let actions: Vec<Json> = loaded
        .manifest
        .as_ref()
        .map(|manifest| {
            manifest
                .actions
                .iter()
                .map(|action| {
                    let (spec, _) = sc_module::config_fields_to_form_fields(
                        &action.config_fields,
                        &action.name,
                    );
                    json!({
                        "name": action.name,
                        "description": action.description,
                        "config_spec": spec_json(&spec, locale),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let functions: Vec<Json> = loaded
        .manifest
        .as_ref()
        .map(|manifest| {
            manifest
                .functions
                .iter()
                .map(|function| {
                    json!({
                        "name": function.name,
                        "description": function.description,
                        "is_async": function.is_async,
                        "arguments": function
                            .arguments
                            .iter()
                            .map(|argument| json!({
                                "name": argument.name,
                                "type": argument.type_name,
                            }))
                            .collect::<Vec<_>>(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": module.id.0,
        "name": module.name,
        // Which language it is written in, and therefore which of the two
        // install forms would reinstall it and whether `permissions` means
        // anything for it (§10: a Python module has no permission model).
        "language": module.language.as_str(),
        "source": module.source.as_str(),
        "location": module.location,
        "version": module.version,
        "configuration": Json::Object(sc_module::redacted_configuration(loaded)),
        // Not redacted, and nothing here is a secret: a permission set is what
        // an admin granted, and the point of the screen is that it can be read.
        "permissions": Json::Object(module.permissions.to_json()),
        "config_spec": spec_json(&loaded.config_spec, locale),
        "actions": actions,
        "functions": functions,
        // The table providers it supplies (§8.3): what the "new table" screen
        // offers, listed here too because the Modules tab is where an admin
        // finds out what installing a module got them.
        "table_providers": loaded.table_provider_names(),
        // The tables it declares (TODO-imap M4): created when it loaded, and
        // kept when it is uninstalled.
        "tables": loaded.table_names(),
        // The application tables it reads because its settings name them
        // (TODO-payments P4.1): a plans table, say. Read only.
        "reads": loaded.read_names(),
        // The services it runs in the background (TODO-imap M5), each with
        // where it is: running, restarting after a failure (and why), or
        // stopped. A module that declares one whose task has not been started
        // — a load that failed — lists it as stopped.
        "services": loaded
            .manifest
            .as_ref()
            .map(|manifest| {
                manifest
                    .services
                    .iter()
                    .map(|service| {
                        let status = statuses.iter().find(|s| s.service == service.name);
                        json!({
                            "name": service.name,
                            "description": service.description,
                            "state": status.map_or(json!("stopped"), |s| json!(s.state)),
                            "restarts": status.map_or(0, |s| s.restarts),
                            "last_error": status.and_then(|s| s.last_error.clone()),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        // The webhooks it receives (TODO-payments P1.2), each with the path it
        // is served at on this host — what the admin registers with the
        // provider. The tab puts its own origin in front: it is served on the
        // admin host, which is where webhooks are.
        "webhooks": loaded
            .manifest
            .as_ref()
            .map(|manifest| {
                manifest
                    .webhooks
                    .iter()
                    .map(|webhook| {
                        json!({
                            "name": webhook.name,
                            "description": webhook.description,
                            "path": sc_module::hook_path(&module.name, &webhook.name),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        // The v1 routes it serves, each with its path on the admin host —
        // where they are served, so the tab puts its own origin in front.
        "routes": loaded
            .manifest
            .as_ref()
            .map(|manifest| {
                manifest
                    .routes
                    .iter()
                    .map(|route| {
                        json!({
                            "method": route.method,
                            "url": route.url,
                            "path": sc_module::route_path(&module.name, &route.url),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        // And the model providers, for the same reason: installing
        // `feldspar-sklearn` gets an admin five estimators on the model form,
        // and the tab is where they find that out.
        "model_providers": loaded.model_provider_names(),
        // And the stream providers (TODO "Streams" §12): what the Streams form
        // offers beside the built-in MQTT one.
        "stream_providers": loaded.stream_provider_names(),
        // And the view patterns (TODO "Saltcorn UI" 11.1): what a Saltcorn UI
        // application's New view offers beside v1's six. One whose name was
        // taken is not here; its issue says so.
        "view_patterns": loaded.view_pattern_names(),
        // The content-security sources it suggests an application allow
        // (TODO-payments P1.4), each switched on per application by its `key`.
        "csp_sources": loaded
            .csp_suggestions()
            .iter()
            .map(sc_module::CspSuggestion::to_json)
            .collect::<Vec<_>>(),
        "unsupported": sc_module::unsupported_json(loaded),
        "issues": loaded.issues,
        "loaded": loaded.is_loaded(),
        "api_version": loaded.manifest.as_ref().and_then(|m| m.api_version),
    })
}

/// One entry of the bundled catalog as JSON (matching `bundled_module_schema`).
///
/// `installed` is decided by the caller against the loaded set rather than read
/// from anything here: the catalog is a directory on disk and knows nothing
/// about what this installation has done with it.
fn bundled_json(entry: &sc_module::BundledModule, installed: bool) -> Json {
    json!({
        "id": entry.id,
        "name": entry.name,
        "language": entry.language.as_str(),
        "title": entry.title,
        "description": entry.description,
        // The tables installing it creates (TODO-imap M4.6), said before the
        // click.
        "tables": entry.tables,
        // And the services it runs (TODO-imap M5): the card says it runs in
        // the background and writes to its own tables.
        "services": entry.services,
        // And the webhooks it receives (TODO-payments P1.2), with the path each
        // will be served at once it is installed.
        "webhooks": entry
            .webhooks
            .iter()
            .map(|name| json!({ "name": name, "path": sc_module::hook_path(&entry.name, name) }))
            .collect::<Vec<_>>(),
        "supplies": entry.supplies,
        "installs": entry.installs,
        // What installing it grants, in the same shape a module's own
        // permissions are reported in — so the card and the module's card draw
        // the same object with the same code.
        "permissions": Json::Object(entry.permissions.to_json()),
        "installed": installed,
    })
}

/// The module services this server was built with, or a configuration error
/// saying it has none — the same shape [`triggers_of`] has, and for the same
/// reason: a test or an admin-only server may have booted without them, and the
/// Modules tab should say so rather than appear to work.
pub(crate) fn modules_of(apps: &AppMounts) -> Result<Arc<crate::ModuleServices>> {
    apps.modules().cloned().ok_or_else(|| {
        Error::config("this server has no module support installed, so modules cannot be managed")
    })
}

/// One run as a list entry (matching `run_summary_schema`): everything except
/// the transcript, which a list of dozens of conversations must not carry.
fn run_summary_json(run: &sc_agent::Run) -> Json {
    json!({
        "id": run.id.0,
        "kind": run.kind.as_str(),
        "subject": run.subject,
        "description": run.description,
        "state": run.state.as_str(),
        "error": run.error,
        "user": run.user,
        "created_at": run.created_at,
        "updated_at": run.updated_at,
        // The workflow half (§10.3), null on an agent run: the version this run
        // is pinned to, the step it is on, and when it next wants the engine.
        "subject_version": run.subject_version,
        "current_step": sc_workflow::run_current_step(run),
        "wake_at": run.wake_at,
        // How an agent run's loop concluded — `{"conclusion": "stuck", "reason":
        // …}` — so a run list can tell an answer from a stuck or over-budget
        // run, which share the `done` state. Null while running and for a
        // workflow run.
        "conclusion": run.conclusion(),
        // The run that delegated this one, null on a run somebody started.
        "parent_run": run.attributes.get(sc_agent::ATTR_PARENT_RUN),
    })
}

/// One whole run (matching `run_schema`): the summary plus the loop state the
/// chat panel reads a transcript out of.
///
/// The workflow half — the trace and the pending form — is empty here and filled
/// in by [`run_json_with_workflow`], which is the one that reads the database.
fn run_json(run: &sc_agent::Run) -> Json {
    let mut out = run_summary_json(run);
    if let Json::Object(fields) = &mut out {
        fields.insert("context".to_owned(), run.context.clone());
        fields.insert(
            "attributes".to_owned(),
            Json::Object(run.attributes.clone()),
        );
        fields.insert("trace".to_owned(), Json::Array(Vec::new()));
        fields.insert("pending_form".to_owned(), Json::Null);
        let plan = match run.kind {
            sc_agent::RunKind::Agent => run
                .agent_loop()
                .ok()
                .and_then(|state| sc_agent_builtins::run_plan(&state)),
            _ => None,
        };
        fields.insert(
            "plan".to_owned(),
            plan.as_ref().map_or(Json::Null, run_plan_json),
        );
    }
    out
}

/// A planner run's plan (matching `run_plan_schema`), every field present.
fn run_plan_json(plan: &sc_agent_builtins::Plan) -> Json {
    let word = |value: Json| value.as_str().unwrap_or_default().to_owned();
    json!({
        "features": plan.features.iter().map(|f| json!({
            "id": f.id,
            "title": f.title,
            "description": f.description,
            "kind": word(json!(f.kind)),
            "acceptance": f.acceptance,
            "files": f.files,
            "pages": f.pages,
            "status": word(json!(f.status)),
            "attempts": f.attempts,
            "runs": f.runs,
        })).collect::<Vec<_>>(),
        "progress": plan.progress.iter().map(|p| json!({
            "feature": p.feature,
            "run": p.run,
            "status": word(json!(p.status)),
            "summary": p.summary,
            "check": p.check,
            "diffstat": p.diffstat,
        })).collect::<Vec<_>>(),
    })
}

/// One whole run, with the workflow half filled in: the `_fd_run_traces` rows a
/// traced workflow wrote, and the form a suspended run is waiting on (§10.3).
///
/// Empty and null on an agent run rather than absent, so one typed shape serves
/// both engines and the client has no union to narrow. The trace rows are read
/// unconditionally for a workflow run because a workflow with tracing off simply
/// has none — asking is one query, and branching on the pinned version's `trace`
/// flag would be a second read to save the first.
async fn run_json_with_workflow(
    catalog: &Catalog,
    run: &sc_agent::Run,
    locale: &Locale,
) -> Result<Json> {
    let mut out = run_json(run);
    if run.kind != sc_agent::RunKind::Workflow {
        return Ok(out);
    }
    let traces = sc_workflow::list_run_traces(catalog, run.id.0).await?;
    // A run whose stored state will not parse is still a row worth showing: the
    // admin looking at it is the person who has to decide what to do about it,
    // and answering an error instead of the run would take the evidence away.
    let form = sc_workflow::run_pending_form(run).unwrap_or(None);
    if let Json::Object(fields) = &mut out {
        fields.insert(
            "trace".to_owned(),
            Json::Array(traces.iter().map(run_trace_json).collect()),
        );
        fields.insert(
            "pending_form".to_owned(),
            match form {
                Some(form) => pending_form_json(&form, locale)?,
                None => Json::Null,
            },
        );
    }
    Ok(out)
}

/// One trace row (matching `run_trace_schema`).
fn run_trace_json(trace: &sc_workflow::RunTrace) -> Json {
    json!({
        "id": trace.id,
        "seq": trace.seq,
        "step": trace.step,
        "started_at": trace.started_at,
        "finished_at": trace.finished_at,
        "attempt": trace.attempt,
        "outcome": trace.outcome.as_str(),
        "error": trace.error,
        "context": Json::Object(trace.context.clone()),
    })
}

/// The form a suspended run is waiting on, lowered to the `FormField`
/// vocabulary the admin UI already renders every other settings form from.
///
/// Lowered here rather than sent as the stored declaration, so `SettingsFields`
/// renders an approval form with no knowledge that workflows exist — and so a
/// field whose declared type nothing recognises is reported *here*, where the
/// admin can read it, rather than as a control nobody can fill in.
fn pending_form_json(form: &sc_workflow::PendingForm, locale: &Locale) -> Result<Json> {
    let fields = form
        .fields
        .iter()
        .map(|f| f.to_form_field())
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "fields": spec_json(&fields, locale),
        "assign_to": form.assign_to,
        "min_role": form.min_role,
    }))
}

/// The agent services this server was built with, or a configuration error.
///
/// Fails loudly rather than answering with an empty list, exactly as
/// [`triggers_of`] does: a process that never installed agents cannot list or
/// save one, and pretending there are none would make a save look like it
/// worked.
pub(crate) fn agents_of(apps: &AppMounts) -> Result<crate::AgentServices> {
    apps.agents().cloned().ok_or_else(|| {
        Error::config("this server has no agents installed, so agents cannot be managed")
    })
}

/// The trigger dispatcher this server was built with, or a configuration error.
///
/// Fails loudly rather than answering with an empty list: a process that never
/// installed triggers (a test, an admin-only server assembled by hand) cannot
/// list, save or run one, and pretending there are none would make a save look
/// like it worked.
pub(crate) fn triggers_of(apps: &AppMounts) -> Result<Arc<sc_action::TriggerDispatcher>> {
    apps.triggers().cloned().ok_or_else(|| {
        Error::config(
            "this server has no trigger dispatcher installed, so triggers cannot be managed",
        )
    })
}

/// Re-project every mounted app that exposes a trigger, after a **restore** —
/// the one caller left, now that an ordinary trigger change re-projects through
/// the dispatcher's [`TriggerObserver`](sc_action::TriggerObserver) instead
/// ([`AppMounts::refresh_triggers`]).
///
/// **Reported, not returned**, which is the one place this differs from the
/// table-settings handlers. An app can genuinely stop projecting: deleting a
/// trigger an app exposes is allowed (blocking it would leave an admin unable to
/// remove a trigger they no longer want), and it makes that app's declaration
/// dangle. The trigger *is* deleted at that point, so failing the request would
/// report the opposite of what happened; the app keeps its previous mount, the
/// operator gets the reason, and the app names the missing trigger when it is
/// next built or mounted.
fn reproject_apps(apps: &AppMounts) {
    if let Err(e) = apps.refresh_triggers() {
        eprintln!(
            "feldspar: the trigger set changed, but an application could not be \
             re-projected and keeps its previous mount: {}",
            sc_error::format_chain(&e)
        );
    }
}

fn parse_app_id(raw: &str) -> Result<AppId> {
    uuid::Uuid::parse_str(raw)
        .map(AppId)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a valid application id")))
}

/// The application a path's `id` names, or the 404 saying there is none.
async fn require_app(catalog: &Catalog, raw_id: &str) -> Result<Application> {
    let id = parse_app_id(raw_id)?;
    load_application(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no application with id {id}")))
}

/// A [`FormField`] as the API returns it (matching `form_field_schema`): enough
/// for the admin UI to render and label a control for it.
/// How many rows a dataset preview reads by default, and the most it will read.
///
/// A preview is a **limited** read rather than a capped one (§9's cap is the
/// fit's bound): previewing is exactly what an admin does to a dataset that
/// turns out to be too big to fit, so refusing to show it would refuse the
/// answer they came for. The default is what a table on a screen shows; the
/// maximum is what a column's *type* can be decided from without the read
/// becoming the thing it was protecting against.
const DATASET_PREVIEW_ROWS: u64 = 20;

/// The model services this server was built with, or a configuration error
/// saying it has none — the same shape [`agents_of`] and [`modules_of`] have,
/// and for the same reason: a test or an admin-only server may have booted
/// without them, and the Models tab should say so rather than appear to work.
pub(crate) fn models_of(apps: &AppMounts) -> Result<crate::ModelServices> {
    apps.models().cloned().ok_or_else(|| {
        Error::config("this server has no model support installed, so models cannot be managed")
    })
}

/// The stream services this server was built with, or a configuration error
/// saying it has none — [`models_of`]'s shape, for [`models_of`]'s reason: a
/// test or an admin-only server may have booted without them, and the Streams
/// tab should say so rather than appear to work.
pub(crate) fn streams_of(apps: &AppMounts) -> Result<crate::StreamServices> {
    apps.streams().cloned().ok_or_else(|| {
        Error::config("this server has no stream support installed, so streams cannot be managed")
    })
}

/// One stored stream as JSON (matching `stream_schema`): the row with its
/// secrets masked, what it would produce, and how it is going.
///
/// The **stored** row is what is rendered, with the supervisor consulted only
/// for status and counters. A stream whose provider was uninstalled has no
/// element type and carries the reason in `error`, and it is still listed and
/// still editable — because editing it is the repair.
pub(crate) fn stream_json(
    streams: &crate::StreamServices,
    apps: &AppMounts,
    stream: &sc_stream::Stream,
) -> Json {
    let registry = streams.registry();
    let redacted = sc_stream::redacted_stream(&registry, stream);
    // Computed, never stored (§5): a copy in the row would be a second answer
    // that drifts the day a provider's declaration changes.
    let (element_type, error) = match registry.get(stream.provider.trim()) {
        Some(provider) => match provider
            .element_type(&stream.configuration)
            .and_then(|ty| ty.validate().map(|()| ty))
        {
            Ok(ty) => (serde_json::to_value(&ty).ok(), None),
            Err(e) => (None, Some(e.to_string())),
        },
        None => (
            None,
            Some(format!(
                "no stream provider named `{}` is registered",
                stream.provider.trim()
            )),
        ),
    };
    let running = streams.supervisor().get(stream.id);
    // How its elements are split into topics (TODO.md "Live updates" §2): the
    // provider's answer for this configuration, `null` when it has none.
    let topics = registry
        .get(stream.provider.trim())
        .and_then(|provider| provider.topic_spec(&stream.configuration).ok())
        .and_then(|spec| serde_json::to_value(spec).ok());
    json!({
        "id": stream.id.0,
        "name": stream.name,
        "description": stream.description,
        "provider": stream.provider,
        "configuration": Json::Object(redacted.configuration),
        "min_role": stream.min_role,
        "attributes": Json::Object(stream.attributes.clone()),
        "enabled": stream.is_enabled(),
        "element_type": element_type,
        "error": error,
        "status": running
            .as_ref()
            .and_then(|r| serde_json::to_value(r.status()).ok()),
        "counters": running.as_ref().map(|r| r.counters().to_json()),
        "topics": topics,
        // Live subscriptions to it from applications' pages right now, on
        // this server (L1.11): beside the counters, and as in-memory as they.
        "subscribers": apps.live().hub().subscribers(stream.id),
        // For a `table_changes` stream, what its per-element checks decided
        // (L3.9).
        "changes": change_counts_json(apps, stream.id),
    })
}

/// What the live hub's fan-outs decided for the `table_changes` stream `id`
/// since this server started — elements × subscriptions checked, delivered,
/// withheld by the read rule, outside the filter, and the time spent deciding
/// readability — or `null` for a stream that has had no such subscriber here.
fn change_counts_json(apps: &AppMounts, id: sc_stream::StreamId) -> Json {
    apps.live()
        .hub()
        .change_counts(id)
        .and_then(|counts| serde_json::to_value(counts).ok())
        .unwrap_or(Json::Null)
}

/// One query parameter read as JSON.
///
/// A dataset in a query string is unusual and deliberate: `listModelProviders`
/// answers what a provider's form looks like *for this dataset*, and the dataset
/// being asked about does not exist anywhere yet — there is no id to name it by.
fn query_json<W>(ctx: &crate::handler::HandlerCtx<W>, name: &str) -> Result<Option<Json>> {
    let Some(raw) = ctx.query_get(name).map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    serde_json::from_str(raw)
        .map(Some)
        .map_err(|e| Error::invalid(format!("`{name}` is not valid JSON: {e}")))
}

/// The dataset's columns and their types, read from the data, or `None` when it
/// cannot be read.
///
/// **`None` is not an error**, and that is the point (task 2.5): the half of
/// validation that needs the shape is the half only a read knows, and a save of
/// a model whose table is momentarily unreadable should still be checked for
/// everything else and still be storable — because editing it is the repair.
pub(crate) async fn dataset_shape(
    models: &crate::ModelServices,
    dataset: &sc_model::Dataset,
) -> Option<sc_model::DatasetShape> {
    let frame = models
        .source()
        .read(
            dataset,
            &sc_model::Read::all(models.max_rows()).first(DATASET_PREVIEW_ROWS),
        )
        .await
        .ok()?;
    Some(sc_model::DatasetShape::of_frame(&dataset.table, &frame))
}

/// The same, for a sentence a **module** declared.
///
/// Which is to say: not translated. A module's strings are its own, in its own
/// package, and this server translating them would be it claiming authorship of
/// text it did not write — the one exclusion the milestone states outright. The
/// declaration carries `module: Some(_)` exactly when that is the case, so the
/// test is the data rather than a convention.
fn say_unless_module(text: &str, module: Option<&String>, locale: &Locale) -> String {
    match module {
        Some(_) => text.to_owned(),
        None => say(text, locale),
    }
}

// --- body accessors ------------------------------------------------------------

/// The caller an admin row endpoint runs as: **role 1** — which clears every
/// policy's role floor, so the admin's own row editor sees and edits every row of
/// a FORCE'd table — carrying the signed-in admin's fields.
///
/// The principal says *who is writing*, which the table event a write raises
/// reports (§10.2); whether the write goes through a policy transaction `rows`
/// decides from the table. The role is explicit rather than the user's own
/// because this endpoint is admin-only, and the row editor must work on a
/// FORCE'd table.
fn admin_caller(user: Option<&sc_auth::User>) -> sc_catalog::Principal {
    sc_access::principal_at(Role::ADMIN, user)
}

#[cfg(test)]
mod coverage_tests;
#[cfg(test)]
mod typed_tests;
