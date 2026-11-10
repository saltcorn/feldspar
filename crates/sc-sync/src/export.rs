//! Exporting the live installation as a [`StructureTree`] (APP_SYNC_PROPOSAL
//! §3.1).
//!
//! **Each record is the admin API's own JSON**, written by the codecs the admin
//! handlers and a backup share (`sc_backup::codec`), so there is no second
//! serialisation to keep in step with the first. Two things are then taken out
//! of it:
//!
//! - **Bookkeeping**: what changes without anyone editing the entity — an
//!   `updated_at`, a trigger's last run, a workflow's version number, an agent's
//!   or trigger's current error — and what is worked out on each read rather
//!   than stored (a table's `rls_available`, an application's build targets).
//!   Left in, an export would differ from the last one with nothing changed.
//! - **What has no meaning on another instance** (§3.4): a local store's
//!   directory, a git store's key and working copy, a checklist's user, a
//!   workspace's author, and every secret, which is the sentinel (§3.3,
//!   [`crate::secrets`]).
//!
//! Rows, users, sessions, runs, history and file store content are never read
//! at all, except a local store's synced code and asset directories (§3.5,
//! [`crate::stores`]).
//!
//! **Two modes** (§7.6). [`Mode::Portable`] is the tree above, for git and
//! zips. [`Mode::Instance`] is what the registry editor shows: the portable
//! tree plus what is this instance's own (§3.4) except `feldspar.toml` and the
//! sync link — every settings section, the build hosts, a store's directory
//! and keys — each marked in [`Export::instance_only`]. Secrets are the
//! sentinel in both.
//!
//! **Export is deterministic**: the same installation exports to the same
//! bytes. Lists that have an order of their own (a table's fields, a
//! workflow's steps) keep it; files are one per entity, named by the entity;
//! and everything else is sorted. Before reading anything it gives every table
//! and field an id ([`crate::identity::ensure_overlays`]), which is the one
//! write an export makes.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value as Json, json};

use sc_backup::codec::{
    agent_json, application_json, backup_db_connection_json, backup_llm_provider_json,
    backup_model_json, backup_module_json, backup_stream_json, constraint_json, field_json,
    library_item_json, page_json, role_json, server_json, table_json, trigger_json, view_json,
    workspace_json,
};
use sc_catalog::{Catalog, FIELD_META_TABLE, Table, list_field_meta_for_table, list_file_stores};
use sc_error::{Error, Result};
use sc_files::{
    ATTR_CLONE_PATH, CFG_DIR, CFG_KEY_PATH, CFG_PATH, CFG_PUBLIC_KEY, FileStoreDef, GIT_BACKEND,
    GitRepo, LOCAL_BACKEND, backend_config_spec,
};
use sc_types::{Attrs, SECRET_SENTINEL};

use crate::format::{FORMAT_FILE, StructureFormat};
use crate::identity::{ensure_overlays, is_structure_table};
use crate::secrets::{Registries, redact_by};
use crate::stores::{StoreWarning, resolve, store_uses, walk_synced};
use crate::tree::{STORE_FILES_DIR, StructureTree, path_segment};

/// Which tree an export writes (§7.6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// The §3 tree: what goes into git and a structure zip.
    #[default]
    Portable,
    /// The portable tree plus this instance's own values (§3.4), which the
    /// registry editor shows and edits and which are never written to git.
    Instance,
}

/// What of a file in an instance tree is this instance's alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum InstanceOnly {
    /// All of it: a settings section that is not portable, a build host.
    Whole,
    /// These values in it, as JSON pointers (`/config/path`).
    Keys {
        /// The pointers.
        keys: Vec<String>,
    },
}

/// What an export produced: the tree, and what Status should say about it.
#[derive(Debug, Clone, Default)]
pub struct Export {
    /// The structure.
    pub tree: StructureTree,
    /// What the file-store rules found worth telling the admin (§3.5).
    pub warnings: Vec<StoreWarning>,
    /// How many overlay rows were created to give a table or field its id.
    pub overlays_created: usize,
    /// In an instance export, the files (by path) that are, or hold values
    /// that are, this instance's alone. Empty in a portable export.
    pub instance_only: BTreeMap<String, InstanceOnly>,
}

/// The settings section an instance export puts the keys no settings form
/// shows under (`sc_config::internal_defs`): the backup schedules, the
/// identity providers, onboarding.
pub const INTERNAL_SECTION: &str = "internal";

/// Internal keys only the server writes — what a backup or a boot last did, a
/// notice — which are bookkeeping and not configuration, and are not in the
/// registry.
const SERVER_WRITTEN: [&str; 4] = [
    sc_config::BACKUP_SCHEDULE_STATUS,
    sc_config::STARTUP_HISTORY,
    sc_config::ADMIN_NOTICES,
    sc_config::RLS_BOUNDARY_REPORTED,
];

/// Keys of a table's JSON that are read off this server rather than stored: what
/// the screen needs to draw a switch, not what the table is.
const TABLE_DERIVED: [&str; 7] = [
    "configured",
    "ownership_error",
    "sync_available",
    "history_available",
    "rls_available",
    "metadata",
    "module_fields",
];

/// Keys of an application's JSON that are worked out from its framework and
/// this machine on each read.
const APPLICATION_DERIVED: [&str; 5] = ["source", "builds", "installs", "has_views", "targets"];

/// Keys of a trigger's JSON that the scheduler, the loader or an approval
/// writes, and that a workflow's version list reports.
const TRIGGER_BOOKKEEPING: [&str; 5] = [
    "error",
    "last_run_at",
    // A person's approval on *this* instance; another instance's admin gives
    // their own.
    "system_approval",
    "workflow_version",
    "workflow_steps",
];

/// The settings of each backend that belong to this instance alone (§3.4).
pub(crate) fn instance_settings(backend: &str) -> &'static [&'static str] {
    match backend {
        LOCAL_BACKEND => &[CFG_PATH],
        GIT_BACKEND => &[CFG_KEY_PATH, CFG_PUBLIC_KEY, CFG_DIR],
        _ => &[],
    }
}

/// Export the installation's structure: the portable tree.
pub async fn export(catalog: &Catalog, registries: &Registries) -> Result<Export> {
    export_mode(catalog, registries, Mode::Portable).await
}

/// Export the installation's structure in `mode`.
pub async fn export_mode(catalog: &Catalog, registries: &Registries, mode: Mode) -> Result<Export> {
    let overlays_created = ensure_overlays(catalog).await?;
    let mut out = Writer {
        mode,
        ..Writer::default()
    };
    out.put(FORMAT_FILE, StructureFormat::current().to_json())?;

    roles(catalog, &mut out).await?;
    modules(catalog, registries, &mut out).await?;
    settings(catalog, &mut out).await?;
    if mode == Mode::Instance {
        build_hosts(catalog, &mut out).await?;
    }
    users(catalog, &mut out).await?;
    tables(catalog, &mut out).await?;
    db_connections(catalog, &mut out).await?;
    let warnings = file_stores(catalog, &mut out).await?;
    code_libraries(catalog, &mut out).await?;
    triggers(catalog, registries, &mut out).await?;
    streams(catalog, registries, &mut out).await?;
    agents(catalog, registries, &mut out).await?;
    llm_providers(catalog, &mut out).await?;
    mcp_servers(catalog, &mut out).await?;
    analytics(catalog, &mut out).await?;
    checklists(catalog, &mut out).await?;
    applications(catalog, &mut out).await?;

    Ok(Export {
        tree: out.tree,
        warnings,
        overlays_created,
        instance_only: out.instance_only,
    })
}

/// The tree under construction, refusing two entities at one path: an export
/// that let the second overwrite the first would lose an entity, and deploying
/// it would delete that entity elsewhere.
#[derive(Default)]
struct Writer {
    tree: StructureTree,
    mode: Mode,
    instance_only: BTreeMap<String, InstanceOnly>,
}

impl Writer {
    fn instance(&self) -> bool {
        self.mode == Mode::Instance
    }

    /// Mark what of `path` is this instance's alone.
    fn mark(&mut self, path: &str, only: InstanceOnly) {
        if matches!(&only, InstanceOnly::Keys { keys } if keys.is_empty()) {
            return;
        }
        self.instance_only.insert(path.to_owned(), only);
    }

    fn put(&mut self, path: impl Into<String>, mut value: Json) -> Result<()> {
        let path = path.into();
        if let Json::Object(map) = &mut value {
            map.remove("updated_at");
        }
        if self.tree.get(&path).is_some() {
            return Err(collision(&path));
        }
        self.tree.insert_json(path, value);
        Ok(())
    }

    fn put_file(&mut self, path: String, bytes: Vec<u8>) -> Result<()> {
        if self.tree.get(&path).is_some() {
            return Err(collision(&path));
        }
        self.tree.insert_file(path, bytes);
        Ok(())
    }
}

fn collision(path: &str) -> Error {
    Error::invalid(format!(
        "two entities export to `{path}`; their names differ only in characters a file \
         name cannot hold — rename one of them"
    ))
}

/// Take `keys` out of an object.
fn strip(value: &mut Json, keys: &[&str]) {
    if let Json::Object(map) = value {
        for key in keys {
            map.remove(*key);
        }
    }
}

fn has_table(catalog: &Catalog, name: &str) -> Result<bool> {
    Ok(catalog.get(name)?.is_some())
}

fn to_json<T: serde::Serialize>(value: &T, what: &str) -> Result<Json> {
    serde_json::to_value(value).map_err(|e| Error::msg(format!("{what}: {e}")))
}

// --- roles, modules, settings, users ------------------------------------------

async fn roles(catalog: &Catalog, out: &mut Writer) -> Result<()> {
    if !has_table(catalog, sc_auth::ROLES_TABLE)? {
        return Ok(());
    }
    let mut roles = sc_auth::list_roles(catalog).await?;
    roles.sort_by_key(|r| r.role.get());
    out.put(
        "roles.json",
        Json::Array(roles.iter().map(role_json).collect()),
    )
}

async fn modules(catalog: &Catalog, registries: &Registries, out: &mut Writer) -> Result<()> {
    if !has_table(catalog, sc_module::MODULES_TABLE)? {
        return Ok(());
    }
    let mut modules = sc_module::list_modules(catalog).await?;
    modules.sort_by(|a, b| a.name.cmp(&b.name));
    let mut list = Vec::with_capacity(modules.len());
    for module in &modules {
        let mut value = backup_module_json(module);
        // A module is matched by its package name (§3.2); the row's id is this
        // instance's.
        strip(&mut value, &["id"]);
        value["configuration"] =
            Json::Object(registries.redact_module(&module.name, &module.configuration));
        list.push(value);
    }
    out.put("modules.json", Json::Array(list))
}

/// The settings sections, each as the values **stored** in it: the portable
/// ones, and in an instance export every other one too, marked.
///
/// Not the values the server acts on (`all_config`): those include what this
/// host pins from its `feldspar.toml`, which is never in the repository
/// (§3.4), and declared defaults, which are the receiving build's to supply.
/// A secret is the sentinel.
async fn settings(catalog: &Catalog, out: &mut Writer) -> Result<()> {
    for (name, fields, portable) in sections(out.mode) {
        let mut values = Map::new();
        for field in fields {
            if let Some(value) = sc_config::stored_config(catalog, field.name()).await? {
                values.insert(field.name().to_owned(), redact_setting(field, value));
            }
        }
        let path = format!("settings/{name}.json");
        if !portable {
            out.mark(&path, InstanceOnly::Whole);
        }
        out.put(path, Json::Object(values))?;
    }
    Ok(())
}

/// The sections `mode` exports, each with its keys and whether it is
/// portable, in screen order.
pub(crate) fn sections(mode: Mode) -> Vec<(&'static str, Vec<&'static sc_types::FormField>, bool)> {
    let fields = |s: &'static sc_config::ConfigSection| s.fields.iter().map(|d| &d.field).collect();
    match mode {
        Mode::Portable => sc_config::portable_sections()
            .iter()
            .map(|s| (s.name, fields(s), true))
            .collect(),
        Mode::Instance => {
            let mut out: Vec<_> = sc_config::config_sections()
                .iter()
                .map(|s| (s.name, fields(s), s.portable))
                .collect();
            // The sections only `portable_sections` names (the maps).
            for section in sc_config::portable_sections() {
                if !out.iter().any(|(name, _, _)| *name == section.name) {
                    out.push((section.name, fields(section), true));
                }
            }
            let claimed: Vec<&str> = out
                .iter()
                .flat_map(|(_, fields, _)| fields.iter().map(|f| f.name()))
                .collect();
            let internal = sc_config::internal_defs()
                .iter()
                .map(|d| &d.field)
                .filter(|f| !claimed.contains(&f.name()) && !SERVER_WRITTEN.contains(&f.name()))
                .collect();
            out.push((INTERNAL_SECTION, internal, false));
            out
        }
    }
}

/// The keys of the settings section `name` of an instance tree, for an
/// editor's schema: `None` for a section there is not.
pub fn settings_section_fields(name: &str) -> Option<Vec<&'static sc_types::FormField>> {
    sections(Mode::Instance)
        .into_iter()
        .find(|(section, _, _)| *section == name)
        .map(|(_, fields, _)| fields)
}

/// A stored setting as an export writes it: a secret as the sentinel, and the
/// identity providers' client secrets each as the sentinel.
fn redact_setting(field: &sc_types::FormField, value: Json) -> Json {
    if field.secret {
        let unset = value.is_null() || value.as_str() == Some("");
        return if unset {
            value
        } else {
            Json::String(SECRET_SENTINEL.to_owned())
        };
    }
    if field.name() == sc_config::OAUTH_PROVIDERS
        && let Json::Array(list) = &value
    {
        let spec = sc_config::authentication::oauth_provider_spec();
        return Json::Array(
            list.iter()
                .map(|item| match item {
                    Json::Object(attrs) => Json::Object(redact_by(Some(&spec), attrs)),
                    other => other.clone(),
                })
                .collect(),
        );
    }
    value
}

/// The build hosts (instance export only): each one's name and targets, never
/// its token.
async fn build_hosts(catalog: &Catalog, out: &mut Writer) -> Result<()> {
    if !has_table(catalog, sc_app::BUILD_HOSTS_TABLE)? {
        return Ok(());
    }
    for host in sc_app::list_build_hosts(catalog).await? {
        let path = format!("build-hosts/{}.json", path_segment(&host.name));
        out.mark(&path, InstanceOnly::Whole);
        out.put(
            path,
            json!({ "id": host.id, "name": host.name, "targets": host.targets }),
        )?;
    }
    Ok(())
}

async fn users(catalog: &Catalog, out: &mut Writer) -> Result<()> {
    let Some(users) = catalog.get(sc_auth::USERS_TABLE)? else {
        return Ok(());
    };
    let ids = field_ids(catalog, &users).await?;
    let fields: Vec<Json> = users
        .fields
        .iter()
        .filter(|f| !sc_auth::is_system_user_column(&f.base.name))
        .map(|f| one_field(f, &ids))
        .collect();
    out.put("users.json", json!({ "fields": fields }))
}

// --- tables -------------------------------------------------------------------

/// Each field's overlay id and description, by name.
async fn field_ids(catalog: &Catalog, table: &Table) -> Result<BTreeMap<String, (String, String)>> {
    if !has_table(catalog, FIELD_META_TABLE)? {
        return Ok(BTreeMap::new());
    }
    Ok(list_field_meta_for_table(catalog, &table.name)
        .await?
        .into_iter()
        .map(|m| (m.field_name, (m.id.0.to_string(), m.description)))
        .collect())
}

fn one_field(field: &sc_catalog::DataField, ids: &BTreeMap<String, (String, String)>) -> Json {
    let (id, description) = ids
        .get(&field.base.name)
        .map(|(id, d)| (Json::String(id.clone()), d.as_str()))
        .unwrap_or((Json::Null, ""));
    let mut value = field_json(field, description);
    value["id"] = id;
    value
}

async fn tables(catalog: &Catalog, out: &mut Writer) -> Result<()> {
    let rls = catalog.primary_info().capabilities().row_level_security;
    let mut tables: Vec<Table> = catalog
        .tables()?
        .into_iter()
        .filter(is_structure_table)
        .collect();
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    for table in &tables {
        let mut value = table_json(catalog, table, rls, &sc_i18n_source());
        strip(&mut value, &TABLE_DERIVED);
        value["id"] = table
            .overlay
            .map_or(Json::Null, |id| Json::String(id.0.to_string()));
        if let Some((module, provider)) = table.provider() {
            let spec = catalog
                .table_provider_kinds()
                .into_iter()
                .find(|k| k.module == module && k.provider == provider)
                .map(|k| k.config_spec);
            value["provider"] = json!({
                "module": module,
                "provider": provider,
                "configuration": Json::Object(redact_by(
                    spec.as_deref(),
                    &sc_backup::codec::provided_configuration(table),
                )),
            });
        }
        let ids = field_ids(catalog, table).await?;
        value["fields"] = Json::Array(table.fields.iter().map(|f| one_field(f, &ids)).collect());
        value["constraints"] = Json::Array(table.constraints.iter().map(constraint_json).collect());
        out.put(format!("tables/{}.json", path_segment(&table.name)), value)?;
    }
    Ok(())
}

/// The source language, which an archive is written in whatever the admin who
/// pressed the button reads.
fn sc_i18n_source() -> sc_i18n::Locale {
    sc_i18n::Locale::source()
}

// --- database connections and file stores --------------------------------------

async fn db_connections(catalog: &Catalog, out: &mut Writer) -> Result<()> {
    if !has_table(catalog, sc_catalog::DB_CONNECTIONS_TABLE)? {
        return Ok(());
    }
    for def in sc_catalog::list_db_connections(catalog).await? {
        let mut value = backup_db_connection_json(&def);
        value["password"] = Json::String(if def.password.is_empty() {
            String::new()
        } else {
            SECRET_SENTINEL.to_owned()
        });
        out.put(
            format!("db-connections/{}.json", path_segment(&def.name)),
            value,
        )?;
    }
    Ok(())
}

async fn file_stores(catalog: &Catalog, out: &mut Writer) -> Result<Vec<StoreWarning>> {
    let mut warnings = Vec::new();
    if !has_table(catalog, sc_catalog::FILE_STORES_TABLE)? {
        return Ok(warnings);
    }
    let uses = store_uses(catalog).await?;
    for def in list_file_stores(catalog).await? {
        let segment = path_segment(&def.name);
        let mut value = store_json(&def, out.mode)?;
        if out.instance() {
            let mut keys: Vec<String> = instance_settings(&def.backend)
                .iter()
                .filter(|k| value["config"].get(**k).is_some())
                .map(|k| format!("/config/{k}"))
                .collect();
            if value.get(ATTR_CLONE_PATH).is_some() {
                keys.push(format!("/{ATTR_CLONE_PATH}"));
            }
            out.mark(
                &format!("file-stores/{segment}.json"),
                InstanceOnly::Keys { keys },
            );
        }
        if def.backend == GIT_BACKEND {
            // The pinned commit: what the working tree has checked out now.
            value["commit"] = match GitRepo::from_def(&def)?.head_commit().await? {
                Some(hash) => Json::String(hash),
                None => Json::Null,
            };
        }
        out.put(format!("file-stores/{segment}.json"), value)?;

        let dirs = resolve(&def, uses.get(&def.name).unwrap_or(&Default::default()))?;
        warnings.extend(dirs.warnings.iter().cloned());
        if dirs.synced.is_empty() {
            continue;
        }
        let root = def
            .setting(CFG_PATH)
            .ok_or_else(|| crate::stores::no_directory(&def))?;
        let (files, found) = walk_synced(std::path::Path::new(root), &dirs);
        warnings.extend(found);
        for (path, bytes) in files {
            if path.contains('\\') {
                warnings.push(StoreWarning::Unreadable {
                    store: def.name.clone(),
                    reason: format!("`{path}` has a backslash in its name"),
                });
                continue;
            }
            out.put_file(
                format!("file-stores/{segment}/{STORE_FILES_DIR}/{path}"),
                bytes,
            )?;
        }
    }
    warnings.sort();
    warnings.dedup();
    Ok(warnings)
}

/// A store's definition: without what is this instance's own in a portable
/// export, and with it — the directory, the key, a git store's clone — in an
/// instance export.
fn store_json(def: &FileStoreDef, mode: Mode) -> Result<Json> {
    let mut config: Attrs = def.config.clone();
    if mode == Mode::Portable {
        for key in instance_settings(&def.backend) {
            config.remove(*key);
        }
    }
    let spec = backend_config_spec(&def.backend).ok();
    let mut value = json!({
        "id": def.id.0,
        "name": def.name,
        "description": def.description,
        "backend": def.backend,
        "config": Json::Object(redact_by(spec.as_deref(), &config)),
        "min_role": def.min_role,
        "sync_contents": def.sync_contents()?.as_str(),
    });
    if mode == Mode::Instance
        && let Some(clone) = def.attributes.get(ATTR_CLONE_PATH)
    {
        value[ATTR_CLONE_PATH] = clone.clone();
    }
    Ok(value)
}

// --- code libraries, triggers, streams, agents ---------------------------------

async fn code_libraries(catalog: &Catalog, out: &mut Writer) -> Result<()> {
    if !has_table(catalog, sc_catalog::CODE_LIBRARIES_TABLE)? {
        return Ok(());
    }
    for library in sc_catalog::list_code_libraries(catalog).await? {
        let value = sc_app::code_library_json(&library, None, None);
        out.put(
            format!("code-libraries/{}.json", path_segment(&library.name)),
            value,
        )?;
    }
    Ok(())
}

async fn triggers(catalog: &Catalog, registries: &Registries, out: &mut Writer) -> Result<()> {
    if !has_table(catalog, sc_action::TRIGGERS_TABLE)? {
        return Ok(());
    }
    let has_versions = has_table(catalog, sc_workflow::VERSIONS_TABLE)?;
    for trigger in sc_action::list_triggers(catalog).await? {
        let mut shown = trigger.clone();
        if let (Some(action), Some(configuration)) = (trigger.action(), trigger.configuration()) {
            let redacted = registries.redact_action(
                catalog,
                action,
                trigger.channel.as_deref(),
                configuration,
            );
            shown.set_configuration(redacted)?;
        }
        let mut value = trigger_json(&shown, None);
        strip(&mut value, &TRIGGER_BOOKKEEPING);
        // A workflow's steps are its current version (§10.3), inline: the
        // history is this instance's.
        if has_versions
            && let Some(mut workflow) = sc_workflow::current_workflow(catalog, trigger.id).await?
        {
            for step in &mut workflow.steps {
                if let sc_workflow::StepKind::Action {
                    action,
                    configuration,
                } = &mut step.kind
                {
                    *configuration = registries.redact_action(
                        catalog,
                        action,
                        trigger.channel.as_deref(),
                        configuration,
                    );
                }
            }
            let mut steps = to_json(&workflow, &format!("workflow `{}`", trigger.name))?;
            strip(&mut steps, &["version"]);
            value["workflow"] = steps;
        }
        out.put(
            format!("triggers/{}.json", path_segment(&trigger.name)),
            value,
        )?;
    }
    Ok(())
}

async fn streams(catalog: &Catalog, registries: &Registries, out: &mut Writer) -> Result<()> {
    if !has_table(catalog, sc_stream::STREAMS_TABLE)? {
        return Ok(());
    }
    for stream in sc_stream::list_streams(catalog).await? {
        let mut value = backup_stream_json(&stream);
        value["configuration"] =
            Json::Object(registries.redact_stream(&stream.provider, &stream.configuration));
        out.put(
            format!("streams/{}.json", path_segment(&stream.name)),
            value,
        )?;
    }
    Ok(())
}

async fn agents(catalog: &Catalog, registries: &Registries, out: &mut Writer) -> Result<()> {
    if !has_table(catalog, sc_agent::AGENTS_TABLE)? {
        return Ok(());
    }
    for mut agent in sc_agent::list_agents(catalog).await? {
        for enabled in &mut agent.traits {
            enabled.config = registries.redact_trait(&enabled.trait_, &enabled.config);
        }
        let mut value = agent_json(&agent, None);
        strip(&mut value, &["error"]);
        out.put(format!("agents/{}.json", path_segment(&agent.name)), value)?;
    }
    Ok(())
}

// --- LLM providers and MCP servers ----------------------------------------------

async fn llm_providers(catalog: &Catalog, out: &mut Writer) -> Result<()> {
    if !has_table(catalog, sc_llm::LLM_PROVIDERS_TABLE)? {
        return Ok(());
    }
    for def in sc_llm::list_llm_providers(catalog).await? {
        // The provider the configuration file supplies is this host's.
        if sc_llm::is_host_llm_provider(catalog, def.id) {
            continue;
        }
        let models = sc_llm::list_llm_models(catalog, &def).await?;
        let mut value = backup_llm_provider_json(&def, &models);
        let spec = sc_llm::provider_config_spec(&def.backend).ok();
        value["config"] = Json::Object(redact_by(spec.as_deref(), &def.config));
        if let Some(Json::Array(models)) = value.get_mut("models") {
            models.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        }
        out.put(
            format!("llm-providers/{}.json", path_segment(&def.name)),
            value,
        )?;
    }
    Ok(())
}

async fn mcp_servers(catalog: &Catalog, out: &mut Writer) -> Result<()> {
    if !has_table(catalog, sc_mcp::MCP_SERVERS_TABLE)? {
        return Ok(());
    }
    for def in sc_mcp::list_mcp_servers(catalog).await? {
        out.put(
            format!("mcp-servers/{}.json", path_segment(&def.name)),
            server_json(&def),
        )?;
    }
    Ok(())
}

// --- analytics and checklists ---------------------------------------------------

async fn analytics(catalog: &Catalog, out: &mut Writer) -> Result<()> {
    if has_table(catalog, sc_dataset::DATASETS_TABLE)? {
        for def in sc_dataset::list_datasets(catalog).await? {
            let value = to_json(&def, &format!("dataset `{}`", def.name))?;
            out.put(
                format!("analytics/datasets/{}.json", path_segment(&def.name)),
                value,
            )?;
        }
    }
    if has_table(catalog, sc_model::MODELS_TABLE)? {
        for model in sc_model::list_models(catalog).await? {
            let value = backup_model_json(catalog, &model).await?;
            out.put(
                format!("analytics/models/{}.json", path_segment(&model.name)),
                value,
            )?;
        }
    }
    if has_table(catalog, sc_analytics::WORKSPACES_TABLE)? {
        for workspace in sc_analytics::list_workspaces(catalog).await? {
            let mut value = workspace_json(&workspace);
            // Who made it is a user of this instance.
            strip(&mut value, &["created_by"]);
            out.put(
                format!(
                    "analytics/workspaces/{}.json",
                    path_segment(&workspace.name)
                ),
                value,
            )?;
        }
    }
    Ok(())
}

async fn checklists(catalog: &Catalog, out: &mut Writer) -> Result<()> {
    if !has_table(catalog, sc_check::CHECKLISTS_TABLE)? {
        return Ok(());
    }
    for checklist in
        sc_check::store::list_checklists(catalog, sc_check::store::ChecklistScope::All).await?
    {
        let steps: Vec<Json> = sc_check::store::current_steps(catalog, checklist.id)
            .await?
            .iter()
            .map(|step| {
                json!({
                    "position": step.position,
                    "type": step.draft.type_,
                    "params": Json::Object(step.draft.params.clone()),
                    "label": step.draft.label,
                    "capture": step.draft.capture,
                })
            })
            .collect();
        // Not `run_as`, a user of this instance, nor `mutates`, which is worked
        // out from the steps.
        let value = json!({
            "id": checklist.id,
            "name": checklist.name,
            "description": checklist.description,
            "application": checklist.application,
            "enabled": checklist.enabled,
            "attributes": Json::Object(checklist.attributes.clone()),
            "steps": steps,
        });
        out.put(
            format!("checklists/{}.json", path_segment(&checklist.name)),
            value,
        )?;
    }
    Ok(())
}

// --- applications -------------------------------------------------------------

async fn applications(catalog: &Catalog, out: &mut Writer) -> Result<()> {
    if !has_table(catalog, sc_app::APPLICATIONS_TABLE)? {
        return Ok(());
    }
    for app in sc_app::list_applications(catalog).await? {
        let dir = format!("applications/{}", path_segment(&app.subdomain));
        let mut value = application_json(&app);
        strip(&mut value, &APPLICATION_DERIVED);
        redact_framework(&mut value["framework"]);
        if let Some(Json::Array(extra)) = value.get_mut("extra_frameworks") {
            extra.iter_mut().for_each(redact_framework);
        }
        out.put(format!("{dir}/application.json"), value)?;

        if has_table(catalog, sc_viewpattern::VIEWS_TABLE)? {
            for view in sc_viewpattern::list_views(catalog, app.id).await? {
                out.put(
                    format!("{dir}/views/{}.json", path_segment(&view.name)),
                    view_json(&view),
                )?;
            }
        }
        if has_table(catalog, sc_viewpattern::PAGES_TABLE)? {
            for page in sc_viewpattern::list_pages(catalog, app.id).await? {
                out.put(
                    format!("{dir}/pages/{}.json", path_segment(&page.name)),
                    page_json(&page),
                )?;
            }
        }
        if has_table(catalog, sc_viewpattern::LIBRARY_TABLE)? {
            for item in sc_viewpattern::list_library(catalog, app.id).await? {
                out.put(
                    format!("{dir}/library/{}.json", path_segment(&item.name)),
                    library_item_json(&item),
                )?;
            }
        }
        // A code application's catalogues are files in its repository and
        // travel with its store; a Saltcorn UI application's are rows.
        if sc_app::app_source_from_config(&app.framework).is_err()
            && has_table(catalog, sc_app::TRANSLATIONS_TABLE)?
        {
            use sc_app::CatalogStore;
            let store = sc_app::RowCatalogStore::new(app.id);
            for locale in store.locales(catalog).await? {
                if let Some(messages) = store.load(catalog, &locale).await? {
                    out.put(
                        format!("{dir}/translations/{}.json", path_segment(locale.as_str())),
                        messages.to_json(),
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// A framework's settings with its secret ones redacted, by its declared spec.
fn redact_framework(framework: &mut Json) {
    let Some(name) = framework.get("name").and_then(Json::as_str) else {
        return;
    };
    let spec = sc_app::framework_config_spec(name).ok();
    if let Some(Json::Object(config)) = framework.get("config") {
        let redacted = redact_by(spec.as_deref(), config);
        framework["config"] = Json::Object(redacted);
    }
}
