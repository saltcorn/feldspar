//! The entity JSON codecs: an application, a table, a trigger, an agent … as
//! the admin API writes it (`*_json`) and reads it back (`*_from_body`).
//!
//! **A backup is the admin API's own JSON, in a zip** (see the crate docs), so
//! these are one set of functions with two users: `sc-server`'s admin handlers
//! answer and accept these shapes, and [`crate::write`] / [`crate::restore`]
//! write and read a backup with the same functions. A field added to an
//! application, an agent or a trigger reaches the backup format the moment it
//! reaches the API, with nothing to remember. The small body-reading helpers
//! the codecs are built from (`str_field`, `optional_bool`, …) are here too,
//! and the handlers use them as well.

use sc_types::Role;
use std::collections::BTreeMap;

use sc_access::rows::require_object;
use sc_action::{
    ATTR_DAY_OF_WEEK, ATTR_HOUR, ATTR_MINUTE, EventKind, Trigger, TriggerBody, TriggerId,
};
use sc_api::schema_edit;
use sc_app::{
    ApiConfig, AppId, Application, CspPolicy, FrameworkRef, StaticDir, StreamRef, TriggerRef,
    app_source_from_config, framework_default_csp,
};
use sc_auth::RoleDef;
use sc_catalog::{
    ATTR_OWNERSHIP_FORMULA, Attrs, Catalog, ConstraintKind, DataField, DataFieldKind,
    DbConnectionDef, DbConnectionId, FieldId, FileOnConflict, FileOnDelete, FileStoreId, Table,
    TableConstraint,
};
use sc_error::{Error, Result};
use sc_files::{
    FileMeta, FileStoreDef, FileStoreDefId, backend_config_spec, backend_operations,
    display_config, run_backend_operation,
};
use sc_i18n::Locale;
use sc_llm::{LlmModelDef, LlmModelDefId, LlmProviderDef, LlmProviderDefId};
use sc_types::{FormField, SECRET_SENTINEL};
use serde_json::{Map, Value as Json, json};

/// A field (column) with its `_fd_fields` overlay merged on (§3.2): the
/// introspected column facts plus the overlay's type, kind, label, description and
/// attributes. `description` is passed in because it lives only in the overlay
/// row, not the merged [`DataField`].
pub fn field_json(field: &DataField, description: &str) -> Json {
    json!({
        "name": field.base.name,
        "label": field.base.label,
        "description": description,
        "sql_type": field.base.type_.sql_type(),
        "type": field.base.type_.name(),
        "nullable": !field.required,
        "required": field.required,
        "unique": field.unique,
        // Introspected, like `unique`: what a field editor needs to say which
        // column a reference onto this table should point at by default.
        "primary_key": field.primary_key,
        // Whether the database fills the column in when a write omits it — an
        // identity key numbering itself, a `uuid` key generating itself. The
        // editor needs it to tell the admin which keys they must type, and the
        // row form needs it to leave a blank one out of the insert instead of
        // sending a null the `NOT NULL` would refuse.
        "generated": field.generated.is_some(),
        // The expression an insert that leaves the field out takes its value
        // from (empty for none) — so the row form can leave a blank one out too.
        "default_expression": field.default_expression.as_deref().unwrap_or(""),
        "kind": field_kind_json(&field.kind),
        "attributes": Json::Object(field.base.attributes.clone()),
    })
}

/// One constraint on the wire. Every kind uses one shape, with the fields that
/// do not apply left null — which is what lets the screen render a list of four
/// different things without four branches for the data and four for the display.
pub fn constraint_json(constraint: &TableConstraint) -> Json {
    let (fields, expression, method, language, formula) = match &constraint.kind {
        ConstraintKind::Unique { fields } => (fields.clone(), None, None, None, None),
        ConstraintKind::Index {
            fields,
            expression,
            method,
        } => (
            fields.clone(),
            expression.clone(),
            Some(method.clone()),
            None,
            None,
        ),
        ConstraintKind::FullTextSearch { language } => {
            (Vec::new(), None, None, Some(language.clone()), None)
        }
        ConstraintKind::Formula { formula } => {
            (Vec::new(), None, None, None, Some(formula.clone()))
        }
        ConstraintKind::VectorIndex { field } => (
            vec![field.clone()],
            None,
            Some(sc_catalog::VECTOR_INDEX_METHOD.to_owned()),
            None,
            None,
        ),
    };
    json!({
        "name": constraint.name,
        "type": constraint.kind.type_name(),
        "fields": fields,
        "expression": expression,
        "method": method,
        "language": language,
        "formula": formula,
        "error_message": constraint.error_message,
        "managed": constraint.is_saltcorn(),
    })
}

/// A field kind as `{ type, …parameters }` — the wire shape `createField` and
/// `updateField` accept back.
pub fn field_kind_json(kind: &DataFieldKind) -> Json {
    match kind {
        DataFieldKind::Plain => json!({ "type": "plain" }),
        DataFieldKind::Key {
            target_table,
            target_field,
            summary_field,
        } => json!({
            "type": "key",
            "target_table": target_table,
            "target_field": target_field.0,
            "summary_field": summary_field.as_ref().map(|f| f.0.clone()),
        }),
        DataFieldKind::File {
            store,
            folder,
            mime_allow,
            on_delete,
            on_conflict,
        } => json!({
            "type": "file",
            "store": store.0,
            "folder": folder,
            "mime_allow": mime_allow,
            "on_delete": on_delete.as_str(),
            "on_conflict": on_conflict.as_str(),
        }),
        DataFieldKind::Calc { expression } => json!({
            "type": "calc",
            "expression": expression,
        }),
        DataFieldKind::Secret => json!({ "type": "secret" }),
    }
}

/// Parse the optional `kind` object of a create/update request into a
/// [`DataFieldKind`], defaulting to `Plain` when absent or null.
pub fn parse_field_kind(obj: &Map<String, Json>) -> Result<DataFieldKind> {
    let Some(value) = obj.get("kind").filter(|v| !v.is_null()) else {
        return Ok(DataFieldKind::Plain);
    };
    let kind = value
        .as_object()
        .ok_or_else(|| Error::invalid("`kind` must be an object"))?;
    match str_field(kind, "type")? {
        "plain" => Ok(DataFieldKind::Plain),
        // A secret (TODO-imap M2): a text column holding encrypted values.
        "secret" => Ok(DataFieldKind::Secret),
        "file" => Ok(DataFieldKind::File {
            store: FileStoreId(non_empty_str_field(kind, "store")?.to_owned()),
            folder: optional_present_str(kind, "folder"),
            mime_allow: string_array_field(kind, "mime_allow")?,
            on_delete: file_option(kind, "on_delete", FileOnDelete::parse)?,
            on_conflict: file_option(kind, "on_conflict", FileOnConflict::parse)?,
        }),
        "key" => Ok(DataFieldKind::Key {
            target_table: non_empty_str_field(kind, "target_table")?.to_owned(),
            target_field: FieldId(non_empty_str_field(kind, "target_field")?.to_owned()),
            summary_field: optional_present_str(kind, "summary_field").map(FieldId),
        }),
        // A non-stored calculated field (Phase 8): a virtual field with no
        // column, only an `sc-expr` expression computed on read.
        "calc" => Ok(DataFieldKind::Calc {
            expression: non_empty_str_field(kind, "expression")?.to_owned(),
        }),
        other => Err(Error::invalid(format!("unknown field kind `{other}`"))),
    }
}

/// A File kind's enumerated option: its default when absent, null or empty.
pub fn file_option<T: Default>(
    kind: &Map<String, Json>,
    key: &str,
    parse: fn(&str) -> Option<T>,
) -> Result<T> {
    match kind.get(key) {
        None | Some(Json::Null) => Ok(T::default()),
        Some(Json::String(s)) => {
            parse(s).ok_or_else(|| Error::invalid(format!("`{key}` does not take `{s}`")))
        }
        Some(other) => Err(Error::invalid(format!(
            "`{key}` should be a string, got {other}"
        ))),
    }
}

/// The field a `createField` body describes, ready for the schema editor.
///
/// Shared with a restore, which is handed exactly the `field_json` a `listFields`
/// would have returned: one parser, so a field kind the editor learns to write is
/// a field kind a backup can carry.
pub fn field_spec_from_body(obj: &Map<String, Json>) -> Result<schema_edit::FieldSpec> {
    Ok(schema_edit::FieldSpec {
        name: non_empty_str_field(obj, "name")?.to_owned(),
        type_name: optional_str(obj, "type"),
        label: optional_str(obj, "label"),
        description: optional_str(obj, "description"),
        required: optional_bool(obj, "required")?,
        unique: optional_bool(obj, "unique")?,
        primary_key: optional_bool(obj, "primary_key")?,
        kind: parse_field_kind(obj)?,
        attributes: attributes_field(obj)?,
        default_expression: Some(optional_str(obj, "default_expression")),
    })
}

/// The whole-object table settings an `updateTable` body carries: every value
/// stated, so an omitted one means cleared (see `updateTable` for why that is the
/// right reading *here* and the wrong one in an agent's batch).
pub fn table_settings_from_body(obj: &Map<String, Json>) -> Result<schema_edit::TableSettings> {
    Ok(schema_edit::TableSettings {
        label: Some(str_field(obj, "label")?.to_owned()),
        description: Some(str_field(obj, "description")?.to_owned()),
        min_role_read: Some(role_field(obj, "min_role_read")?),
        min_role_write: Some(role_field(obj, "min_role_write")?),
        ownership_formula: Some(str_field(obj, "ownership_formula")?.to_owned()),
        rls_enabled: Some(bool_field(obj, "rls_enabled")?),
        sync_enabled: Some(bool_field(obj, "sync_enabled")?),
        // Absent reads as off, like every other omitted value of this body.
        // Optional on the wire only so a body written before the switch existed
        // (an older backup's `table.json`) still restores.
        history_enabled: Some(optional_bool(obj, "history_enabled")?),
        // Absent is the admin's, the default (§14.12).
        history_min_role: Some(match obj.get("history_min_role") {
            None | Some(Json::Null) => Role::ADMIN,
            Some(_) => role_field(obj, "history_min_role")?,
        }),
        // Absent keeps history forever, the default.
        history_keep_days: Some(match obj.get("history_keep_days") {
            None | Some(Json::Null) => 0,
            Some(days) => days
                .as_u64()
                .and_then(|d| u32::try_from(d).ok())
                .ok_or_else(|| {
                    Error::invalid("`history_keep_days` must be a whole number of days")
                })?,
        }),
    })
}

pub fn application_json(app: &Application) -> Json {
    json!({
        "id": app.id.to_string(),
        "name": app.name,
        "description": app.description,
        "subdomain": app.subdomain,
        "framework": framework_ref_json(&app.framework),
        "extra_frameworks": app.extra_frameworks.iter().map(framework_ref_json).collect::<Vec<_>>(),
        "tables": app.tables.to_vec(),
        "api_tables": app.api_tables.to_vec(),
        "file_stores": app.file_stores.iter().map(|s| s.0.clone()).collect::<Vec<_>>(),
        "triggers": app.triggers.iter().map(|t| t.0.clone()).collect::<Vec<_>>(),
        "streams": app.streams.iter().map(|s| s.0.clone()).collect::<Vec<_>>(),
        "apis": app.apis.iter().map(|a| json!({ "provider": a.provider, "mount": a.mount, "config": Json::Object(a.config.clone()) })).collect::<Vec<_>>(),
        "static_dirs": app.static_dirs.iter().map(|d| json!({ "mount": d.mount, "store": d.store.0, "path": d.path })).collect::<Vec<_>>(),
        "uploads": sc_app::upload_slots_json(&app.uploads),
        "agents": sc_app::agent_exposures_json(&app.agents),
        "csp": csp_json(&app.csp),
        "attributes": Json::Object(app.attributes.clone()),
        "min_role": app.min_role,
        "max_role": app.max_role,
        "access_if": app.access_if,
        "source": app_source_json(app),
        // A framework constructed from `sc-app`'s factory registry has nothing
        // to build: saving is its deployment (TODO "Saltcorn UI" 9.3).
        "builds": sc_app::framework_factory(&app.framework.name).is_none(),
        "installs": sc_app::framework_factory(&app.framework.name).is_none()
            && app_source_from_config(&app.framework).is_ok_and(|s| s.build.install.is_some()),
        "has_views": app.framework.name == sc_viewpattern::SALTCORN_UI_FRAMEWORK,
        "targets": sc_app::framework_build_targets(&app.framework.name)
            .into_iter()
            .map(|t| {
                // A state of this machine, next to the target's description: what
                // the button needs to warn before it is pressed. A target whose
                // paths do not resolve for this app has nothing to check; its
                // build says why.
                let readiness = sc_app::app_target_readiness(app, &t.name).unwrap_or_default();
                json!({
                    "name": t.name,
                    "label": t.label,
                    "readiness": {
                        "ready": readiness.is_ready(),
                        "missing": readiness.missing,
                        "from_environment": readiness.from_environment,
                    },
                })
            })
            .collect::<Vec<_>>(),
    })
}

/// Where the app's source lives, as `{ store, path }` — or `null` for a framework
/// with no source tree.
///
/// **Derived here rather than read off the config**, because how a framework
/// spells its source is the framework's business: `code` states it in `store` +
/// `source`, `react` derives it from `project`. Resolving it server-side is what
/// lets the admin UI link into the file manager at an app's source with no
/// per-framework code in the screen (§2.4). A config that does not resolve is not
/// an error here: this is a convenience field on a row that is being listed, and
/// the *reason* it does not resolve is reported where it belongs — on save, or on
/// build.
///
/// A `none` application has no source to build but does have a directory its
/// coding agent writes in, and that is the one worth a link.
pub fn app_source_json(app: &Application) -> Json {
    match app_source_from_config(&app.framework) {
        Ok(source) => json!({
            "store": source.store.0,
            "path": source.build.source_dir,
        }),
        Err(_) => match sc_app::none_source_dir(&app.framework) {
            Some((store, path)) => json!({ "store": store, "path": path }),
            None => Json::Null,
        },
    }
}

/// A Saltcorn UI view as `{ id, name, description, viewpattern, table_name,
/// configuration, min_role, slug, attributes }`: the shape a backup carries one
/// in, and the one the Views tab will list (TODO "Saltcorn UI" 9.1). The
/// configuration is v1's, unchanged.
pub fn view_json(view: &sc_viewpattern::View) -> Json {
    json!({
        "id": view.id.0.to_string(),
        "name": view.name,
        "description": view.description,
        "viewpattern": view.viewpattern,
        "table_name": view.table_name,
        "configuration": Json::Object(view.configuration.clone()),
        "min_role": view.min_role,
        "slug": view.slug,
        "attributes": Json::Object(view.attributes.clone()),
    })
}

/// Parse a view in [`view_json`]'s shape into `application`. What it may name —
/// the pattern, the table, the role, the actions — is `save_view`'s to check.
pub fn view_from_body(
    id: sc_viewpattern::ViewId,
    application: sc_app::AppId,
    body: &Json,
) -> Result<sc_viewpattern::View> {
    let obj = require_object(body)?;
    let name = non_empty_str_field(obj, "name")?;
    let viewpattern = non_empty_str_field(obj, "viewpattern")?;
    let mut view = sc_viewpattern::View::new(application, name, viewpattern, "");
    view.id = id;
    view.description = obj
        .get("description")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_owned();
    view.table_name = obj
        .get("table_name")
        .and_then(Json::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    view.configuration = object_member(obj, "configuration")?;
    view.min_role = min_role_member(obj)?;
    view.slug = obj.get("slug").filter(|s| !s.is_null()).cloned();
    view.attributes = object_member(obj, "attributes")?;
    Ok(view)
}

/// A Saltcorn UI page as `{ id, name, title, description, layout, min_role,
/// attributes }` — [`view_json`]'s counterpart.
pub fn page_json(page: &sc_viewpattern::Page) -> Json {
    json!({
        "id": page.id.0.to_string(),
        "name": page.name,
        "title": page.title,
        "description": page.description,
        "layout": page.layout,
        "min_role": page.min_role,
        "attributes": Json::Object(page.attributes.clone()),
    })
}

/// Parse a page in [`page_json`]'s shape into `application`.
pub fn page_from_body(
    id: sc_viewpattern::PageId,
    application: sc_app::AppId,
    body: &Json,
) -> Result<sc_viewpattern::Page> {
    let obj = require_object(body)?;
    let name = non_empty_str_field(obj, "name")?;
    let mut page = sc_viewpattern::Page::new(application, name)
        .title(obj.get("title").and_then(Json::as_str).unwrap_or(""))
        .layout(obj.get("layout").cloned().unwrap_or_else(|| json!({})));
    page.id = id;
    page.description = obj
        .get("description")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_owned();
    page.min_role = min_role_member(obj)?;
    page.attributes = object_member(obj, "attributes")?;
    Ok(page)
}

/// A library item as `{ id, name, description, icon, layout, attributes }` —
/// the page pair's counterpart for `_fd_library` (TODO "The builder" §8).
pub fn library_item_json(item: &sc_viewpattern::LibraryItem) -> Json {
    json!({
        "id": item.id.0.to_string(),
        "name": item.name,
        "description": item.description,
        "icon": item.icon,
        "layout": item.layout,
        "attributes": Json::Object(item.attributes.clone()),
    })
}

/// Parse a library item in [`library_item_json`]'s shape into `application`.
/// Whether the application may have a library is `save_library_item`'s to check.
pub fn library_item_from_body(
    id: sc_viewpattern::LibraryItemId,
    application: sc_app::AppId,
    body: &Json,
) -> Result<sc_viewpattern::LibraryItem> {
    let obj = require_object(body)?;
    let name = non_empty_str_field(obj, "name")?;
    let mut item = sc_viewpattern::LibraryItem::new(application, name)
        .icon(obj.get("icon").and_then(Json::as_str).unwrap_or(""))
        .description(obj.get("description").and_then(Json::as_str).unwrap_or(""))
        .layout(obj.get("layout").cloned().unwrap_or_else(|| json!({})));
    item.id = id;
    item.attributes = object_member(obj, "attributes")?;
    Ok(item)
}

/// An object member, absent or `null` meaning the empty object.
pub fn object_member(obj: &Map<String, Json>, key: &str) -> Result<Attrs> {
    match obj.get(key) {
        None | Some(Json::Null) => Ok(Attrs::new()),
        Some(Json::Object(o)) => Ok(o.clone()),
        Some(_) => Err(Error::invalid(format!("`{key}` must be an object"))),
    }
}

/// A view's or page's `min_role`, absent meaning public.
pub fn min_role_member(obj: &Map<String, Json>) -> Result<Role> {
    match obj.get("min_role") {
        None | Some(Json::Null) => Ok(Role::PUBLIC),
        Some(value) => value
            .as_u64()
            .and_then(|r| Role::try_from(r).ok())
            .ok_or_else(|| Error::invalid("`min_role` must be a role number from 1 to 100")),
    }
}

/// A [`FrameworkRef`] as `{ name, config }`.
pub fn framework_ref_json(fw: &FrameworkRef) -> Json {
    json!({ "name": fw.name, "config": Json::Object(fw.config.clone()) })
}

/// A [`CspPolicy`] as a directive→sources object.
pub fn csp_json(csp: &CspPolicy) -> Json {
    Json::Object(
        csp.directives
            .iter()
            .map(|(name, sources)| {
                (
                    name.clone(),
                    Json::Array(sources.iter().map(|s| Json::String(s.clone())).collect()),
                )
            })
            .collect(),
    )
}

pub fn application_from_body(id: AppId, body: &Json) -> Result<Application> {
    let obj = require_object(body)?;
    let name = non_empty_str_field(obj, "name")?.to_owned();
    let subdomain = non_empty_str_field(obj, "subdomain")?.to_owned();
    let description = obj
        .get("description")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_owned();
    let framework = parse_framework(
        obj.get("framework")
            .ok_or_else(|| Error::invalid("missing field `framework`"))?,
    )?;
    let extra_frameworks = parse_array(obj, "extra_frameworks")?
        .iter()
        .map(parse_framework)
        .collect::<Result<_>>()?;
    let tables = parse_str_array(obj, "tables")?.into_iter().collect();
    // Absent is "serves no table's row API" — the default a new application
    // has; each table is opted in by name, and must be one of `tables`.
    let api_tables = parse_str_array(obj, "api_tables")?.into_iter().collect();
    let file_stores = parse_str_array(obj, "file_stores")?
        .into_iter()
        .map(FileStoreId)
        .collect();
    // Absent means "expose nothing": the field arrived after applications did,
    // and an older client that does not send it must not have its app's exposed
    // subset silently rewritten — `parse_str_array` already reads a missing
    // field as the empty list.
    let triggers = parse_str_array(obj, "triggers")?
        .into_iter()
        .map(TriggerRef)
        .collect();
    // Read the same way, for the same reason.
    let streams = parse_str_array(obj, "streams")?
        .into_iter()
        .map(StreamRef)
        .collect();
    let apis = parse_array(obj, "apis")?
        .iter()
        .map(|v| {
            let o = require_object(v)?;
            // The mount is required rather than defaulted: an empty one
            // normalises to `/`, which claims every path — a blank field is far
            // more likely a slip than a request for that.
            //
            // The settings are optional and validated on save against the spec
            // the provider declares (`validate_api_config`), so a key nobody
            // declared is refused here rather than stored and ignored.
            Ok(ApiConfig::new(
                non_empty_str_field(o, "provider")?,
                non_empty_str_field(o, "mount")?,
            )
            .with_config(object_field(o, "config")?))
        })
        .collect::<Result<_>>()?;
    let static_dirs = parse_array(obj, "static_dirs")?
        .iter()
        .map(|v| {
            let o = require_object(v)?;
            Ok(StaticDir::new(
                str_field(o, "mount")?,
                FileStoreId(non_empty_str_field(o, "store")?.to_owned()),
                str_field(o, "path")?,
            ))
        })
        .collect::<Result<_>>()?;
    // Absent is "no upload slots", read as the triggers are.
    let uploads = sc_app::upload_slots_from_json(obj.get("uploads"))?;
    // …and so are the exposed agents (§11.7).
    let agents = sc_app::agent_exposures_from_json(obj.get("agents"))?;
    let csp = parse_csp(obj.get("csp"), &framework.name)?;
    let attributes = match obj.get("attributes") {
        None | Some(Json::Null) => Attrs::new(),
        Some(Json::Object(o)) => o.clone(),
        Some(_) => return Err(Error::invalid("`attributes` must be an object")),
    };
    // Absent is "let everybody in, as they are": the defaults a new application
    // has. The ranges and the formula are checked on save.
    let app_role = |key: &str, default: Role| -> Result<Role> {
        match obj.get(key) {
            None | Some(Json::Null) => Ok(default),
            Some(value) => value
                .as_u64()
                .and_then(|r| Role::try_from(r).ok())
                .ok_or_else(|| {
                    Error::invalid(format!("`{key}` should be a role number, 1 to 100"))
                }),
        }
    };
    let min_role = app_role("min_role", Role::PUBLIC)?;
    let max_role = app_role("max_role", Role::ADMIN)?;
    let access_if = match obj.get("access_if") {
        None | Some(Json::Null) => String::new(),
        Some(Json::String(formula)) => formula.trim().to_owned(),
        Some(_) => return Err(Error::invalid("`access_if` should be a formula, as text")),
    };

    Ok(Application {
        id,
        name,
        description,
        subdomain,
        framework,
        extra_frameworks,
        tables,
        api_tables,
        file_stores,
        triggers,
        streams,
        apis,
        static_dirs,
        uploads,
        agents,
        csp,
        min_role,
        max_role,
        access_if,
        attributes,
    })
}

/// Parse a `{ name, config }` framework reference; a missing/null `config` is an
/// empty settings bag.
pub fn parse_framework(value: &Json) -> Result<FrameworkRef> {
    let obj = require_object(value)?;
    let name = non_empty_str_field(obj, "name")?.to_owned();
    let config = match obj.get("config") {
        None | Some(Json::Null) => Attrs::new(),
        Some(Json::Object(o)) => o.clone(),
        Some(_) => {
            return Err(Error::invalid(format!(
                "framework `{name}` config must be an object"
            )));
        }
    };
    Ok(FrameworkRef { name, config })
}

/// Parse a `csp` directive→sources object; absent means the framework's default
/// policy ([`framework_default_csp`]).
///
/// The default is the framework's rather than a fixed `strict` because a
/// framework that also chooses the build tooling knows what that tooling's output
/// needs — a scaffolded React app gets a policy fitted to a Vite bundle, and the
/// admin who is no longer picking the bundler is not asked to derive the policy
/// for it either. A stated `csp` always wins: this is a default, not a fixture.
pub fn parse_csp(value: Option<&Json>, framework: &str) -> Result<CspPolicy> {
    match value {
        None | Some(Json::Null) => Ok(framework_default_csp(framework)),
        Some(Json::Object(o)) => {
            let mut directives = BTreeMap::new();
            for (name, sources) in o {
                let Some(items) = sources.as_array() else {
                    return Err(Error::invalid(format!(
                        "csp directive `{name}` must be a list of sources"
                    )));
                };
                let sources = items
                    .iter()
                    .map(|s| {
                        s.as_str().map(str::to_owned).ok_or_else(|| {
                            Error::invalid(format!(
                                "csp directive `{name}` has a non-string source"
                            ))
                        })
                    })
                    .collect::<Result<_>>()?;
                directives.insert(name.clone(), sources);
            }
            Ok(CspPolicy { directives })
        }
        Some(_) => Err(Error::invalid("`csp` must be an object")),
    }
}

/// An optional JSON-array field, defaulting to empty when absent or null.
pub fn parse_array<'a>(obj: &'a Map<String, Json>, key: &str) -> Result<&'a [Json]> {
    match obj.get(key) {
        None | Some(Json::Null) => Ok(&[]),
        Some(Json::Array(items)) => Ok(items),
        Some(_) => Err(Error::invalid(format!("field `{key}` must be an array"))),
    }
}

/// An optional array-of-strings field (table / store subsets), defaulting empty.
pub fn parse_str_array(obj: &Map<String, Json>, key: &str) -> Result<Vec<String>> {
    parse_array(obj, key)?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::invalid(format!("field `{key}` must be an array of strings")))
        })
        .collect()
}

/// Parse an application id from a path segment.
/// A file store as the API returns it (matching `file_store_schema`): its stored
/// definition plus the live state only the running server knows.
///
/// `connected`/`error`/`is_git_repo` are read from the catalog rather than the
/// row, because they are not properties of the definition: the same definition
/// is connected on one machine and not on another, and that difference is
/// exactly what the admin needs to see.
pub fn file_store_json(catalog: &Catalog, def: &FileStoreDef) -> Result<Json> {
    let git = catalog.file_store_is_git_repo(&def.name)?;
    Ok(json!({
        "id": Some(def.id.0),
        "name": def.name,
        "description": def.description,
        "backend": def.backend,
        // Redacted here, where the record is serialised, rather than in the
        // screen (§11.1): every reader of this endpoint gets the same treatment,
        // including one written later that never thought about keys. No backend
        // declares a secret today — the git backend's `key_path` is a path and
        // its `public_key` is public — so this is currently a no-op, and that is
        // the point: an S3 backend's secret is redacted by declaring it, not by
        // editing this function.
        //
        // `display_config` first, for the other direction: a setting the admin
        // left blank and the backend then settled — a git store's working-copy
        // directory — is shown as what it actually is rather than as the blank
        // that is stored. It is read-only on the edit form, so a blank one would
        // leave the admin with nowhere to learn where their files are.
        "config": Json::Object(shown_config(def)),
        "min_role": def.min_role,
        "connected": git.is_some(),
        // Only meaningful when not connected; a connected store has had any
        // recorded reason cleared, so this is null for a working store.
        "error": catalog.file_store_error(&def.name)?,
        // A property of the instance, so null when there is no instance to ask.
        "is_git_repo": git,
    }))
}

/// A stored store's config as the admin should see it: what the backend decided
/// for itself filled in ([`display_config`]), then its
/// [`secret`](FormField::secret) settings replaced by the sentinel.
///
/// An unknown backend redacts nothing rather than failing: this is called while
/// *listing*, and a store whose backend a plugin used to supply must still be
/// listable and editable so the admin can repoint it. There is nothing to leak
/// in that case either — no spec means no field is declared secret.
pub fn shown_config(def: &FileStoreDef) -> sc_types::Attrs {
    let config = display_config(def);
    match backend_config_spec(&def.backend) {
        Ok(spec) => sc_types::redact_attrs(&spec, &config),
        Err(_) => config,
    }
}

/// Run every operation the backend declares as part of **creating** a store,
/// before anything is written (`Operation::on_create` — a git store's clone).
///
/// A failure propagates, and that is the point: the caller has not saved yet, so
/// a store whose contents could not be created is a store that does not exist,
/// and the admin gets the backend's own message back in the form they are still
/// looking at.
///
/// Backend-agnostic: `local` declares no `on_create` operation and so does
/// nothing here — its one operation configures the form rather than creating
/// anything — and a backend added later, an object store creating its bucket,
/// gets the same transactional create by declaring one.
pub async fn create_backend_resources(def: &mut FileStoreDef) -> Result<()> {
    for op in backend_operations(&def.backend)? {
        if op.on_create {
            run_backend_operation(def, &op.name, &sc_types::Attrs::new()).await?;
        }
    }
    Ok(())
}

/// A JSON object field of a request body, defaulting to empty when absent —
/// which is what a caller sends for an operation that takes no arguments.
pub fn object_field(obj: &Map<String, Json>, key: &str) -> Result<sc_types::Attrs> {
    match obj.get(key) {
        Some(Json::Object(o)) => Ok(o.clone()),
        None | Some(Json::Null) => Ok(sc_types::Attrs::new()),
        Some(_) => Err(Error::invalid(format!("field `{key}` must be an object"))),
    }
}

/// A file's metadata as the API returns it (matching `file_meta_schema`).
///
/// Reports both the rule set on this entry and the `effective_min_role` that
/// actually applies — they differ whenever a parent directory or the store
/// itself is more restrictive, and showing only the former would let an admin
/// believe a file is reachable when its folder has locked it.
pub fn file_meta_json(path: &str, meta: &FileMeta, effective: Option<Role>) -> Json {
    json!({
        "path": path,
        "min_role": meta.min_role,
        "effective_min_role": effective,
        "attributes": meta
            .attributes
            .iter()
            .map(|(k, v)| (k.clone(), Json::String(v.clone())))
            .collect::<Map<String, Json>>(),
    })
}

/// An optional role field of a body: absent or null means unrestricted, and a
/// value outside the 1–100 scale is rejected rather than clamped.
pub fn optional_role(obj: &Map<String, Json>, key: &str) -> Result<Option<Role>> {
    match obj.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(value) => {
            let raw = value
                .as_i64()
                .ok_or_else(|| Error::invalid(format!("field `{key}` must be a number or null")))?;
            Role::try_from(raw)
                .ok()
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "field `{key}` must be a role between 1 and 100, got {raw}"
                    ))
                })
                .map(Some)
        }
    }
}

/// Build a [`FileStoreDef`] from a create/update body, with `id` supplied by the
/// caller (minted on create, taken from the path on update).
///
/// The backend's settings are *not* validated here: `save_file_store` checks
/// them against the backend's declared spec, which is the single place that
/// knows how, and doing it twice would risk the two drifting.
/// A store's definition for a **backup**: the `createFileStore` shape, with the
/// backend settings *not* redacted.
///
/// The one place a secret is written out deliberately. `file_store_json` redacts,
/// as everything that answers a browser must; a backup restored onto a fresh
/// server has to be able to connect the store, and a definition carrying the
/// sentinel where its credential was is a store that will not. The zip is
/// therefore as sensitive as the database it came from, which the Backup screen
/// says beside the choice.
pub fn backup_store_def_json(def: &FileStoreDef) -> Json {
    json!({
        "name": def.name,
        "description": def.description,
        "backend": def.backend,
        "config": Json::Object(def.config.clone()),
        "min_role": def.min_role,
    })
}

/// One file's stored metadata for a backup: the rules that were *set*, not the
/// effective ones.
///
/// `file_meta_json` reports `effective_min_role` too, which is the right answer
/// for a screen and the wrong one to restore: it is computed from the
/// directories above the file, so writing it back would turn an inherited rule
/// into a rule of its own.
///
/// `mode` is the file's permission bits where the store is a directory on disk,
/// so a restore gives an executable script back its executable bit; `None` (an
/// object store, or a platform without Unix modes) leaves it out.
pub fn backup_file_meta_json(
    path: &str,
    size: Option<u64>,
    meta: &FileMeta,
    mode: Option<u32>,
) -> Json {
    json!({
        "path": path,
        // What a person reading `store.json` wants to know about a file before
        // unpacking it; the restore reads the bytes, not this.
        "size": size,
        "mode": mode,
        "min_role": meta.min_role,
        // Who created the file travels with it. It is an id, so it only means
        // anything on a restore into the same installation — but dropping it
        // would orphan every restored file even there.
        "owner": meta.owner,
        "attributes": meta
            .attributes
            .iter()
            .map(|(k, v)| (k.clone(), Json::String(v.clone())))
            .collect::<Map<String, Json>>(),
    })
}

/// A file's metadata back from a backup, in the shape `setFileMeta` accepts.
pub fn backup_file_meta_from_json(value: &Json) -> Result<(String, FileMeta)> {
    let obj = require_object(value)?;
    let path = non_empty_str_field(obj, "path")?.to_owned();
    let attributes = match obj.get("attributes") {
        Some(Json::Object(o)) => o
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|text| (k.clone(), text.to_owned())))
            .collect::<BTreeMap<String, String>>(),
        _ => BTreeMap::new(),
    };
    Ok((
        path,
        FileMeta {
            min_role: optional_role(obj, "min_role")?,
            // Restored with the bytes: a backup that forgot who owned each file
            // would silently orphan every one of them.
            owner: obj.get("owner").and_then(Json::as_str).map(str::to_owned),
            attributes,
        },
    ))
}

/// The table a trigger fires on, or `None` when it is not a table event.
///
/// The channel *is* the table for `insert`/`update`/`delete` (and
/// `insert_or_update`) and is something
/// else entirely for a channel-based or scheduled trigger, so the distinction is
/// made once, here, rather than at each place that needs to ask.
pub fn trigger_table(trigger: &Trigger) -> Option<&str> {
    match trigger.when {
        when if when.is_table_event() => trigger.channel.as_deref(),
        _ => None,
    }
}

pub fn file_store_from_body(id: FileStoreDefId, body: &Json) -> Result<FileStoreDef> {
    let obj = require_object(body)?;
    let name = non_empty_str_field(obj, "name")?.to_owned();
    let backend = non_empty_str_field(obj, "backend")?.to_owned();
    let description = obj
        .get("description")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_owned();
    let config = match obj.get("config") {
        Some(Json::Object(o)) => o.clone(),
        // A backend with no settings may be posted without a config at all.
        None | Some(Json::Null) => sc_types::Attrs::new(),
        Some(_) => return Err(Error::invalid("field `config` must be an object")),
    };
    // Absent or null means unrestricted. A value outside the role scale is
    // rejected rather than clamped — clamping would quietly change who can
    // reach the store, which is the opposite of what the admin asked for.
    let min_role = match obj.get("min_role") {
        None | Some(Json::Null) => None,
        Some(value) => {
            let raw = value
                .as_i64()
                .ok_or_else(|| Error::invalid("field `min_role` must be a number or null"))?;
            Some(Role::try_from(raw).ok().ok_or_else(|| {
                Error::invalid(format!(
                    "field `min_role` must be a role between 1 and 100, got {raw}"
                ))
            })?)
        }
    };

    Ok(FileStoreDef {
        id,
        name,
        description,
        backend,
        config,
        min_role,
        attributes: sc_types::Attrs::new(),
    })
}

/// Build a [`DbConnectionDef`] from a request body, restoring the stored
/// password when the body echoes the sentinel back.
///
/// `existing` is the stored row on an update (and on a test of a saved
/// connection), `None` on a create. Passing it is what makes "save the form
/// without retyping the password" work, and passing `None` on a create is what
/// makes a literal sentinel there mean itself rather than a lookup that would
/// find nothing.
pub fn db_connection_from_body(
    id: DbConnectionId,
    body: &Json,
    existing: Option<&DbConnectionDef>,
) -> Result<DbConnectionDef> {
    let obj = require_object(body)?;
    let name = non_empty_str_field(obj, "name")?.trim().to_owned();
    let str_field = |key: &str| -> String {
        obj.get(key)
            .and_then(Json::as_str)
            .unwrap_or("")
            .trim()
            .to_owned()
    };

    // Absent, null or zero means "the Postgres default", which is what an admin
    // who left the box alone meant. Anything outside the TCP range is rejected
    // rather than clamped.
    let port = match obj.get("port") {
        None | Some(Json::Null) => sc_catalog::DEFAULT_PORT,
        Some(value) => {
            let raw = value
                .as_i64()
                .ok_or_else(|| Error::invalid("field `port` must be a number"))?;
            if raw == 0 {
                sc_catalog::DEFAULT_PORT
            } else {
                u16::try_from(raw).map_err(|_| {
                    Error::invalid(format!("field `port` must be a TCP port, got {raw}"))
                })?
            }
        }
    };

    let submitted = obj
        .get("password")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_owned();
    let password = match (submitted.as_str(), existing) {
        (SECRET_SENTINEL, Some(existing)) => existing.password.clone(),
        // A sentinel with nothing stored behind it is not a password anybody
        // meant to set; it is a form that was shown one for a connection this
        // request is not about. Empty is the safe reading.
        (SECRET_SENTINEL, None) => sc_error::Secret::default(),
        (other, _) => other.into(),
    };

    // A body that names no backend means Postgres, which is what every
    // connection was before there was a second kind.
    let backend = match str_field("backend").as_str() {
        "" => sc_catalog::POSTGRES_BACKEND.to_owned(),
        other => other.to_owned(),
    };
    let sqlite = backend == sc_catalog::SQLITE_BACKEND;
    let schema = str_field("schema");
    Ok(DbConnectionDef {
        id,
        backend,
        name,
        description: str_field("description"),
        host: str_field("host"),
        port,
        database: str_field("database"),
        username: str_field("username"),
        password,
        // A SQLite database has one namespace and no name for it, so the
        // default is not applied there: an empty schema is the truth about the
        // connection rather than a box the admin forgot to fill in.
        schema: match (schema.is_empty(), sqlite) {
            (true, false) => sc_catalog::DEFAULT_SCHEMA.to_owned(),
            _ => schema,
        },
        file_store: str_field("file_store"),
        file_path: str_field("file_path"),
        attributes: sc_types::Attrs::new(),
    })
}

/// An LLM provider for a **backup**: the `createLlmProvider` shape with the
/// config *not* redacted, and the provider's models beside it.
///
/// The second place a secret is written out deliberately, for the reason
/// [`backup_store_def_json`] gives: a provider restored with the sentinel where
/// its API key was is a provider no agent can call through, and an admin who
/// restores a backup expects the agents in it to answer.
pub fn backup_llm_provider_json(def: &LlmProviderDef, models: &[LlmModelDef]) -> Json {
    json!({
        "id": def.id.0,
        "name": def.name,
        "description": def.description,
        "backend": def.backend,
        "config": Json::Object(def.config.clone()),
        "models": models.iter().map(|m| llm_model_json(def, m)).collect::<Vec<_>>(),
    })
}

/// A module for a **backup**: its row, configuration and permissions included,
/// so a restore can reinstall the package from where it came and hand it back
/// the settings — a password among them — and the reach the admin granted it.
pub fn backup_module_json(module: &sc_module::Module) -> Json {
    json!({
        "id": module.id.0,
        "name": module.name,
        "language": module.language.as_str(),
        "source": module.source.as_str(),
        "location": module.location,
        "version": module.version,
        "configuration": Json::Object(module.configuration.clone()),
        "permissions": Json::Object(module.permissions.to_json()),
        "attributes": Json::Object(module.attributes.clone()),
    })
}

/// A module back from a backup, not yet installed: the name is the backup's,
/// and the install that follows is what says whether it is still true.
pub fn module_from_backup(value: &Json) -> Result<sc_module::Module> {
    let obj = require_object(value)?;
    let source = sc_module::ModuleSource::parse(non_empty_str_field(obj, "source")?)?;
    let mut module = sc_module::Module::new(
        non_empty_str_field(obj, "name")?,
        source,
        non_empty_str_field(obj, "location")?,
    )
    .in_language(match obj.get("language").and_then(Json::as_str) {
        Some(language) => sc_module::ModuleLanguage::parse(language)?,
        None => sc_module::ModuleLanguage::JavaScript,
    });
    if let Some(id) = obj
        .get("id")
        .and_then(Json::as_str)
        .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
    {
        module.id = sc_module::ModuleId(id);
    }
    module.configuration = object_or_empty(obj, "configuration")?;
    module.attributes = object_or_empty(obj, "attributes")?;
    if let Some(permissions) = obj.get("permissions").filter(|v| !v.is_null()) {
        module.permissions = sc_module::ModulePermissions::from_json(permissions)?;
    }
    Ok(module)
}

/// A connection to another database for a **backup**: the
/// `createDatabaseConnection` shape with the password as it is stored, for the
/// reason [`backup_store_def_json`] gives.
pub fn backup_db_connection_json(def: &DbConnectionDef) -> Json {
    json!({
        "id": def.id.0,
        "backend": def.backend,
        "name": def.name,
        "description": def.description,
        "host": def.host,
        "port": def.port,
        "database": def.database,
        "username": def.username,
        "password": def.password.expose(),
        "schema": def.schema,
        "file_store": def.file_store,
        "file_path": def.file_path,
    })
}

/// A stream for a **backup**: the `saveStream` shape with its configuration
/// unredacted — a broker password replaced by the sentinel is a stream that
/// will not connect.
pub fn backup_stream_json(stream: &sc_stream::Stream) -> Json {
    json!({
        "id": stream.id.0,
        "name": stream.name,
        "description": stream.description,
        "provider": stream.provider,
        "configuration": Json::Object(stream.configuration.clone()),
        "min_role": stream.min_role,
        "attributes": Json::Object(stream.attributes.clone()),
        "enabled": stream.is_enabled(),
    })
}

/// A model for a **backup**: the `saveModel` body — datasets by id, which a
/// restore puts back first — plus the view state, which no save carries.
pub async fn backup_model_json(catalog: &Catalog, model: &sc_model::Model) -> Result<Json> {
    Ok(json!({
        "id": model.id.0,
        "name": model.name,
        "description": model.description,
        "provider": model.provider,
        "dataset": { "dataset_id": model.dataset.id },
        "related": model
            .related
            .iter()
            .map(|r| json!({ "name": r.name, "dataset_id": r.dataset.id, "label": r.label }))
            .collect::<Vec<_>>(),
        "configuration": Json::Object(model.configuration.clone()),
        "hyperparameters": Json::Object(model.hyperparameters.clone()),
        "split": serde_json::to_value(model.split)
            .map_err(|e| Error::msg(format!("split: {e}")))?,
        "attributes": Json::Object(model.attributes.clone()),
        "view_state": Json::Object(sc_model::model_view_state(catalog, model.id).await?),
    }))
}

/// An optional object field, empty when absent or null.
pub fn object_or_empty(obj: &Map<String, Json>, key: &str) -> Result<sc_types::Attrs> {
    match obj.get(key) {
        None | Some(Json::Null) => Ok(sc_types::Attrs::new()),
        Some(Json::Object(map)) => Ok(map.clone()),
        Some(_) => Err(Error::invalid(format!("field `{key}` must be an object"))),
    }
}

/// Rebuild a provider definition from a create/update body.
pub fn llm_provider_from_body(id: LlmProviderDefId, body: &Json) -> Result<LlmProviderDef> {
    let obj = require_object(body)?;
    Ok(LlmProviderDef {
        id,
        name: non_empty_str_field(obj, "name")?.to_owned(),
        description: obj
            .get("description")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned(),
        backend: non_empty_str_field(obj, "backend")?.to_owned(),
        config: object_field(obj, "config")?,
        attributes: sc_types::Attrs::new(),
    })
}

/// A model row as the API returns it (matching `llm_model_schema`): the row,
/// plus the capabilities and prices it resolves to on its provider's backend.
pub fn llm_model_json(provider: &LlmProviderDef, model: &LlmModelDef) -> Json {
    json!({
        "id": model.id.0,
        "provider_id": model.provider_id.0,
        "name": model.name,
        "description": model.description,
        "kind": model.kind.as_str(),
        "is_default": model.is_default,
        "config": Json::Object(model.config.clone()),
        // Only a chat model has capabilities: the loop's to know.
        "capabilities": if model.kind == sc_llm::ModelKind::Chat {
            serde_json::to_value(model.capabilities(&provider.backend)).unwrap_or(Json::Null)
        } else {
            Json::Null
        },
        "prices": serde_json::to_value(model.prices()).unwrap_or(Json::Null),
    })
}

/// Rebuild a model row from a create/update body.
pub fn llm_model_from_body(
    id: LlmModelDefId,
    provider: LlmProviderDefId,
    body: &Json,
) -> Result<LlmModelDef> {
    let obj = require_object(body)?;
    Ok(LlmModelDef {
        id,
        provider_id: provider,
        name: non_empty_str_field(obj, "name")?.trim().to_owned(),
        description: obj
            .get("description")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned(),
        kind: sc_llm::ModelKind::parse(obj.get("kind").and_then(Json::as_str).unwrap_or(""))?,
        is_default: obj
            .get("is_default")
            .and_then(Json::as_bool)
            .unwrap_or(false),
        config: match obj.get("config") {
            None | Some(Json::Null) => sc_types::Attrs::new(),
            Some(_) => object_field(obj, "config")?,
        },
        attributes: sc_types::Attrs::new(),
    })
}

/// One stored agent as JSON (matching `agent_schema`), with the reason it cannot
/// run when there is one (§11.2 — a broken agent stays listed and editable).
pub fn agent_json(agent: &sc_agent::Agent, problem: Option<String>) -> Json {
    json!({
        "id": agent.id.0,
        "name": agent.name,
        "description": agent.description,
        "provider": agent.provider,
        "model": agent.model,
        "system_prompt": agent.system_prompt,
        "traits": agent
            .traits
            .iter()
            .map(|enabled| json!({
                "trait": enabled.trait_,
                "config": Json::Object(enabled.config.clone()),
            }))
            .collect::<Vec<_>>(),
        "min_role": agent.min_role,
        "attributes": Json::Object(agent.attributes.clone()),
        "error": problem,
    })
}

/// Rebuild an agent definition from a create/update body.
///
/// Nothing is defaulted quietly: an absent `traits` is an agent with no traits,
/// which is a real agent (one that only talks), while a `traits` of the wrong
/// shape is a refusal naming the entry — an agent half-read is one that would
/// answer with the wrong tools.
pub fn agent_from_body(id: sc_agent::AgentId, body: &Json) -> Result<sc_agent::Agent> {
    let obj = require_object(body)?;
    let mut agent = sc_agent::Agent::with_id(
        id,
        non_empty_str_field(obj, "name")?,
        non_empty_str_field(obj, "provider")?,
    )
    .description(optional_str(obj, "description"))
    .system_prompt(optional_str(obj, "system_prompt"));
    // Absent, null or blank all mean "the provider's own default model", which
    // is a real answer rather than a missing one — a form posts "" for a box it
    // left alone.
    if let Some(model) = obj.get("model").and_then(Json::as_str)
        && !model.trim().is_empty()
    {
        agent = agent.model(model.trim());
    }
    if let Some(traits) = obj.get("traits").filter(|v| !v.is_null()) {
        let Json::Array(entries) = traits else {
            return Err(Error::invalid("field `traits` must be an array"));
        };
        for (i, entry) in entries.iter().enumerate() {
            let Json::Object(entry) = entry else {
                return Err(Error::invalid(format!(
                    "trait {} must be an object of `trait` and `config`",
                    i + 1
                )));
            };
            let name = entry
                .get("trait")
                .and_then(Json::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| Error::invalid(format!("trait {} must name a trait", i + 1)))?;
            agent = agent.with_trait(
                sc_agent::EnabledTrait::new(name.trim())
                    .configuration(object_field(entry, "config")?),
            );
        }
    }
    if let Some(min_role) = obj.get("min_role").filter(|v| !v.is_null()) {
        let raw = min_role
            .as_i64()
            .ok_or_else(|| Error::invalid("field `min_role` must be a number"))?;
        agent = agent.min_role(Role::try_from(raw).map_err(|_| {
            Error::invalid(format!(
                "`min_role` must be a role between 1 and 100, got {raw}"
            ))
        })?);
    }
    // The sparse per-agent values (§9): whatever the form set, nothing it did
    // not. An empty bag means every one of them is the provider's default.
    agent.attributes = object_field(obj, "attributes")?;
    Ok(agent)
}

/// Parse a UUID path parameter, naming what it was meant to identify.
pub fn parse_uuid(raw: &str, what: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(raw)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a valid {what} id")))
}

/// The posted document as a [`Workflow`](sc_workflow::Workflow) of `id`.
///
/// The document's own `id` and `version` are **the server's**: a payload does not
/// get to reassign which workflow it is, and which version it is, is the store's
/// answer (`save_workflow` reads the maximum and mints the next). Everything else
/// is read strictly through the stored serde shape, so what the editor may send
/// and what the engine will run cannot drift apart.
pub fn workflow_from_document(
    id: sc_action::TriggerId,
    posted: &Json,
) -> Result<sc_workflow::Workflow> {
    let mut document = match posted {
        Json::Object(fields) => fields.clone(),
        _ => return Err(Error::invalid("field `workflow` must be a workflow object")),
    };
    document.insert("id".to_owned(), json!(id.0));
    document.insert("version".to_owned(), json!(1));
    serde_json::from_value(Json::Object(document))
        .map_err(|e| Error::invalid(format!("this is not a workflow the engine can read: {e}")))
}

/// One stored trigger as JSON, with the reason it is not usable when there is
/// one (§10.2 — a broken trigger stays listed and editable).
pub fn trigger_json(trigger: &Trigger, problem: Option<String>) -> Json {
    trigger_json_with(trigger, problem, None)
}

/// The same, plus the live version of a **workflow** body: which version is
/// current and how many steps it has.
///
/// A separate entry point rather than a fifth argument on every caller, because
/// only the list has the answer to hand: creating or updating a trigger has just
/// written the row and has not read the versions table, and answering `null`
/// there is honest — the editor is the next screen, and it asks.
pub fn trigger_json_with(
    trigger: &Trigger,
    problem: Option<String>,
    workflow: Option<(u32, usize)>,
) -> Json {
    let mut out = json!({
        "id": trigger.id.0,
        "name": trigger.name,
        "description": trigger.description,
        "when": trigger.when.as_str(),
        "channel": trigger.channel,
        "only_if": trigger.only_if,
        // Which engine runs it, and — for an action body — what it runs with.
        // A workflow answers `null` for both: its steps are a version of their
        // own, fetched with `getWorkflow` (§10.3).
        "body": trigger.body.as_str(),
        "action": trigger.action(),
        "configuration": trigger.configuration().map(|c| Json::Object(c.clone().into_iter().collect())),
        "min_role": trigger.min_role,
        "enabled": trigger.is_enabled(),
        // The timing, read back the way it was posted: absent is null, not 0, so
        // "never set" and "set to midnight" stay distinguishable in the form.
        "minute": trigger.attributes.get(ATTR_MINUTE),
        "hour": trigger.attributes.get(ATTR_HOUR),
        "day_of_week": trigger.attributes.get(ATTR_DAY_OF_WEEK),
        "error": problem,
        // The scheduler's record, RFC 3339 — what the list shows so an admin can
        // see that a nightly job is actually running.
        "last_run_at": trigger.last_run_at.map(|t| t.to_rfc3339()),
        // The workflow half, null on an action body and on a workflow whose
        // versions the caller did not read.
        "workflow_version": Json::Null,
        "workflow_steps": Json::Null,
    });
    if let (Some(map), Some((version, steps))) = (out.as_object_mut(), workflow) {
        map.insert("workflow_version".to_owned(), json!(version));
        map.insert("workflow_steps".to_owned(), json!(steps));
    }
    out
}

/// A trigger from a request body, under the id the caller's route decided.
///
/// Only the shape is checked here — that the fields are present and of the right
/// kind. Whether the *values* mean anything (the event exists, the table exists,
/// the action is registered and configured the way it declares, the `only_if`
/// resolves) is `validate_trigger`'s, called by `save_trigger`, so there is one
/// authority for it and the admin gets the same message the loader would.
pub fn trigger_from_body(id: TriggerId, body: &Json) -> Result<Trigger> {
    let obj = require_object(body)?;
    let when = EventKind::parse(non_empty_str_field(obj, "when")?)?;
    // The body decides what the rest of the request means: an `action` body
    // needs an action name and may carry a configuration, and a `workflow` body
    // has neither. Absent is `action`, because that is what every trigger was
    // before §10.3 and what a client that has not heard of workflows sends.
    let kind = obj
        .get("body")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .unwrap_or(TriggerBody::ACTION);
    let action = match kind {
        TriggerBody::WORKFLOW => None,
        _ => Some(non_empty_str_field(obj, "action")?.to_owned()),
    };
    let configuration = match obj.get("configuration").filter(|v| !v.is_null()) {
        Some(Json::Object(config)) => config.clone().into_iter().collect(),
        Some(_) => return Err(Error::invalid("field `configuration` must be an object")),
        None => sc_types::Attrs::new(),
    };
    // One reading of the three, the same one the stored row goes through, so the
    // API refuses exactly what the loader refuses.
    let body = TriggerBody::parse(kind, action.as_deref(), configuration)?;
    let mut trigger = Trigger::with_body(id, non_empty_str_field(obj, "name")?, when, body)
        .description(optional_str(obj, "description"));
    // Absent, null, or blank are all "no channel" — a form posts the empty
    // string for a picker it did not show.
    if let Some(channel) = obj.get("channel").and_then(Json::as_str)
        && !channel.trim().is_empty()
    {
        trigger = trigger.on(channel.trim());
    }
    if let Some(only_if) = obj.get("only_if").and_then(Json::as_str)
        && !only_if.trim().is_empty()
    {
        trigger = trigger.only_if(only_if.trim());
    }
    if let Some(min_role) = obj.get("min_role").filter(|v| !v.is_null()) {
        let raw = min_role
            .as_i64()
            .ok_or_else(|| Error::invalid("field `min_role` must be a number"))?;
        trigger = trigger.min_role(Role::try_from(raw).map_err(|_| {
            Error::invalid(format!(
                "`min_role` must be a role between 1 and 100, got {raw}"
            ))
        })?);
    }
    // Absent means enabled, as the stored attribute does: a trigger is created
    // to run.
    if obj.get("enabled").and_then(Json::as_bool) == Some(false) {
        trigger.set_enabled(false);
    }
    // The periodic timing (§10.2). A null or absent value is *not* stored, which
    // is what keeps the attributes sparse (§9) and what lets `Schedule::of`
    // refuse a value on a kind that has no use for it — a form posts null for an
    // input it did not show, exactly as it does for `channel`.
    for key in [ATTR_MINUTE, ATTR_HOUR, ATTR_DAY_OF_WEEK] {
        if let Some(value) = obj.get(key).filter(|v| !v.is_null()) {
            let n = value
                .as_u64()
                .ok_or_else(|| Error::invalid(format!("field `{key}` must be a whole number")))?;
            trigger.attributes.insert(key.to_owned(), json!(n));
        }
    }
    // `last_run_at` is deliberately not read: it is the scheduler's record of
    // what happened, and an edit must not be able to rewrite history.
    Ok(trigger)
}

/// A [`Stream`](sc_stream::Stream) from a `saveStream` body.
///
/// The id in the body is what says create or replace, as `saveModel`'s is. A
/// secret setting may arrive as the sentinel; putting the stored value back
/// behind it is the caller's job, because only the caller knows which row this
/// is replacing.
pub fn stream_from_body(body: &Map<String, Json>) -> Result<sc_stream::Stream> {
    let id = match body.get("id") {
        None | Some(Json::Null) => sc_stream::StreamId::new(),
        Some(Json::String(raw)) => sc_stream::StreamId(parse_uuid(raw, "stream")?),
        Some(_) => return Err(Error::invalid("`id` must be a stream id")),
    };
    let mut stream = sc_stream::Stream::with_id(
        id,
        non_empty_str_field(body, "name")?,
        non_empty_str_field(body, "provider")?,
    );
    stream.description = optional_str(body, "description");
    stream.configuration = optional_attrs(body, "configuration")?;
    stream.attributes = optional_attrs(body, "attributes")?;
    stream.min_role = match body.get("min_role") {
        None | Some(Json::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(|n| Role::try_from(n).ok())
                .ok_or_else(|| Error::invalid("`min_role` must be a role between 1 and 100"))?,
        ),
    };
    // Absent means enabled: a stream an admin has just filled the broker
    // details in for is one they want running, and a second switch to press
    // before finding out whether the details were right is the wrong default.
    stream.set_enabled(match body.get("enabled") {
        None | Some(Json::Null) => true,
        Some(Json::Bool(enabled)) => *enabled,
        Some(_) => return Err(Error::invalid("`enabled` must be true or false")),
    });
    Ok(stream)
}

/// A named dataset off the wire — `{ "dataset_id": "…" }` — resolved against
/// the catalog and every stored dataset (analytics TODO A1.8).
pub async fn dataset_from_json(catalog: &Catalog, value: &Json) -> Result<sc_model::Dataset> {
    let id = sc_model::dataset_ref(value)?;
    let schema = sc_dataset::Schema::of_catalog(catalog)?;
    let library = sc_dataset::load_library(catalog).await?;
    Ok(sc_model::Dataset::resolve(&schema, &library, id))
}

/// A [`Model`](sc_model::Model) from a `saveModel` body.
///
/// The id in the body is what says create or replace, and it is the only place
/// an id is read from a payload here: there is no path-addressed update, because
/// the form always sends the whole definition and two endpoints would be one
/// behaviour under two names.
pub async fn model_from_body(
    catalog: &Catalog,
    body: &Map<String, Json>,
) -> Result<sc_model::Model> {
    let id = match body.get("id") {
        None | Some(Json::Null) => sc_model::ModelId::new(),
        Some(Json::String(raw)) => sc_model::ModelId(parse_uuid(raw, "model")?),
        Some(_) => return Err(Error::invalid("`id` must be a model id")),
    };
    let dataset = dataset_from_json(
        catalog,
        body.get("dataset")
            .ok_or_else(|| Error::invalid("`dataset` is required"))?,
    )
    .await?;
    let mut model = sc_model::Model::with_id(
        id,
        non_empty_str_field(body, "name")?,
        non_empty_str_field(body, "provider")?,
        dataset,
    );
    model.description = optional_str(body, "description");
    model.configuration = optional_attrs(body, "configuration")?;
    model.hyperparameters = optional_attrs(body, "hyperparameters")?;
    model.attributes = optional_attrs(body, "attributes")?;
    if let Some(related) = body.get("related").filter(|v| !v.is_null()) {
        let items = related.as_array().ok_or_else(|| {
            Error::invalid("`related` is a list of `{ name, dataset_id, label }`")
        })?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let name = item
                .get("name")
                .and_then(Json::as_str)
                .ok_or_else(|| Error::invalid("a related dataset needs a `name`"))?;
            let mut named =
                sc_model::NamedDataset::new(name, dataset_from_json(catalog, item).await?);
            named.label = item
                .get("label")
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned);
            out.push(named);
        }
        model.related = out;
    }
    if let Some(split) = body.get("split").filter(|v| !v.is_null()) {
        model.split = serde_json::from_value(split.clone())
            .map_err(|e| Error::invalid(format!("`split` is not a split: {e}")))?;
    }
    Ok(model)
}

/// An optional JSON-object field, empty when absent or null.
pub fn optional_attrs(body: &Map<String, Json>, key: &str) -> Result<Attrs> {
    match body.get(key) {
        None | Some(Json::Null) => Ok(Attrs::new()),
        Some(Json::Object(map)) => Ok(map.clone()),
        Some(_) => Err(Error::invalid(format!("`{key}` must be a JSON object"))),
    }
}

/// A declared spec as the admin API returns it — **translated** (§16.1, D5).
///
/// The one place a `config_spec` becomes JSON, and therefore the one place the
/// rule lives: the server translates everything the server says, so a stream
/// provider's `Broker URL` reaches a French admin as `URL du courtier` without
/// the SPA holding a catalogue of labels it did not write. The clone is the
/// price of translating a declaration that is borrowed from a registry and
/// shared by every request; it is one small `Vec` per form.
///
/// On a monolingual installation this is one failed lookup per field and the
/// labels come back unchanged (D11).
/// A declared sentence — a framework's one-line pitch, a provider's label, an
/// action's description — in the request's locale (§16.1, D5).
///
/// [`spec_json`]'s companion, and the same rule with the same key: the declared
/// English. The extractor reads a `label:`/`description:` where it is written,
/// so these reach the catalogue by the same route a `FormField`'s label does.
pub fn say(text: &str, locale: &Locale) -> String {
    sc_i18n::core::translate(locale, text, &[])
}

pub fn spec_json(spec: &[FormField], locale: &Locale) -> Vec<Json> {
    let mut spec = spec.to_vec();
    sc_types::translate_spec(&mut spec, locale);
    spec.iter().map(form_field_json).collect()
}

pub fn form_field_json(field: &FormField) -> Json {
    let type_name = field
        .base
        .type_
        .as_basic()
        .map(|b| b.name())
        .unwrap_or("text");
    json!({
        "name": field.base.name,
        "label": field.base.label,
        "type": type_name,
        "required": field.required,
        "default": field.default.clone().unwrap_or(Json::Null),
        "options": field.static_options(),
        "option_labels": field.option_labels,
        "multiline": field.multiline,
        "secret": field.secret,
        "create_only": field.create_only,
        "code_language": field.code_language,
        "section": field.section,
        "sublabel": field.sublabel,
        "help_topic": field.help_topic,
        "pattern": field.pattern,
        "pattern_error": field.pattern_error,
        "env_fallback": env_fallback_json(field),
        "show_if": field
            .show_if
            .iter()
            .map(|c| json!({ "name": c.setting, "values": c.allowed }))
            .collect::<Vec<_>>(),
    })
}

/// The variable a blank `field` leaves the job to, with its value in the
/// server's environment now (null when unset). A secret's value is not shown.
pub fn env_fallback_json(field: &FormField) -> Json {
    let Some(name) = &field.env_fallback else {
        return Json::Null;
    };
    let value = std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let value = match value {
        Some(_) if field.secret => Some(sc_types::SECRET_SENTINEL.to_owned()),
        value => value,
    };
    json!({ "name": name, "value": value })
}

/// A required string field of an object body.
/// A table as the admin UI sees it: its name plus the `_fd_tables` overlay
/// merged onto it (§9).
///
/// `configured` is the overlay's *presence*, not its content. A table an admin
/// deliberately set to admin-only and one nobody has ever opened both report
/// `1`/`1`; only the first has a row, and only the first can be "forgotten".
pub fn table_json(catalog: &Catalog, table: &Table, rls_available: bool, locale: &Locale) -> Json {
    json!({
        "name": table.name,
        "label": table.label,
        "description": table.description,
        "min_role_read": table.access.min_role_read,
        "min_role_write": table.access.min_role_write,
        "configured": table.overlay.is_some(),
        // The live formula's source — or, when the stored source failed to
        // parse/validate at merge time, still that source (from attributes) so
        // the admin edits what they typed, with `ownership_error` saying what
        // is wrong and `ownership` granting nothing meanwhile (fail closed).
        "ownership_formula": table.ownership.as_ref().map(|f| f.source().to_owned())
            .or_else(|| table.attributes.get(ATTR_OWNERSHIP_FORMULA)
                .and_then(Json::as_str).map(str::to_owned))
            .unwrap_or_default(),
        "ownership_error": table.ownership_error,
        "rls_enabled": table.rls_enabled,
        "sync_enabled": table.sync_enabled(),
        // The same rule a save is refused by, so the switch is never a trap.
        "sync_available": sc_catalog::change_log_key(catalog, table, sc_catalog::ChangeLogFeature::Sync)
            .is_ok(),
        "history_enabled": table.history_enabled(),
        "history_min_role": sc_catalog::history::history_min_role(&table.attributes),
        "history_keep_days": sc_catalog::retention::history_keep_days(&table.attributes),
        // Row history needs the same one-column key on Saltcorn's own database.
        "history_available": sc_catalog::change_log_key(catalog, table, sc_catalog::ChangeLogFeature::History)
            .is_ok(),
        // The backend's capability *and* whose database this is: RLS is DDL
        // against the primary driver, so it is never available on a table a
        // connection contributed, nor on a **provided** table, whose rows are
        // not in any database. The SPA renders the toggle from this, and a
        // toggle that can only ever be refused is not a setting, it is a trap.
        "rls_available": rls_available
            && table.database == sc_catalog::DbId::primary()
            && table.provider().is_none()
            && !table.is_metadata(),
        // Which database hosts it: `primary` for Saltcorn's own, otherwise the
        // name of the connection an admin added. The list screen shows anything
        // that is not the primary as a badge beside the table's name, because a
        // list mixing two databases and saying so nowhere would be a list an
        // admin could act on wrongly.
        "database": table.database.0,
        "metadata": table.is_metadata(),
        // The module that declared it and the fields it declared (TODO-imap
        // M4.3): a badge on the list, and fields the editor will not delete.
        "module": catalog.module_tables().module_of(&table.name),
        "module_fields": catalog.module_tables().get(&table.name)
            .map(|c| c.fields.clone()).unwrap_or_default(),
        // The table provider serving its rows, or null (§8.3).
        "provider": provided_json(catalog, table, locale),
    })
}

/// A provided table's definition as the table page reads it: which provider,
/// what it was configured with, what it asks for, and what is wrong.
///
/// `null` for a table in a database, which is most of them.
///
/// The **declaration travels with the values** because the settings form on the
/// table's own page needs both and one round trip is what it has — and because
/// the declaration is not stored anywhere: it is read from the package at load,
/// so a module upgraded this morning asks for what it asks for now rather than
/// for what it asked for when the table was made.
pub fn provided_json(catalog: &Catalog, table: &Table, locale: &Locale) -> Json {
    let Some((module, provider)) = table.provider() else {
        return Json::Null;
    };
    let config_spec = catalog
        .table_provider_kinds()
        .into_iter()
        .find(|k| k.module == module && k.provider == provider)
        .map(|k| k.config_spec)
        .unwrap_or_default();
    let issues: Vec<String> = catalog
        .provided_table_issues()
        .into_iter()
        .filter(|issue| issue.table == table.name)
        .map(|issue| issue.problem)
        .collect();
    json!({
        "module": module,
        "provider": provider,
        // Which of v1's three write methods `get_table` answered for these
        // settings (§8.3). The data screens draw their buttons from it: a
        // provider configured read-only offers a viewer, and one that answers
        // `insertRow` and nothing else offers "Add row" without "Delete".
        "writes": {
            "insert": table.provided_writes().insert,
            "update": table.provided_writes().update,
            "delete": table.provided_writes().delete,
        },
        // Redacted the way every other secret is: a provider's `password` field
        // comes back as the sentinel, and submitting it unchanged keeps what is
        // stored (`sc_types::merge_secrets`, applied on the way in).
        "configuration": Json::Object(sc_types::redact_attrs(
            &config_spec,
            &provided_configuration(table),
        )),
        "config_spec": spec_json(&config_spec, locale),
        "issues": issues,
    })
}

/// The configuration stored on a provided table's row.
pub fn provided_configuration(table: &Table) -> sc_types::Attrs {
    match table.attributes.get(sc_catalog::ATTR_PROVIDER_CONFIG) {
        Some(Json::Object(map)) => map.clone(),
        _ => sc_types::Attrs::new(),
    }
}

/// A required role field of an object body: an integer on the `1..=100` scale.
///
/// Rejects rather than clamps, for the reason the storage layer does: the
/// nearest legal role is still a decision about who reaches the data, and it is
/// not one the server gets to make on the admin's behalf.
pub fn role_field(obj: &Map<String, Json>, key: &str) -> Result<Role> {
    let raw = int_field(obj, key)?;
    Role::try_from(raw).map_err(|_| {
        Error::invalid(format!(
            "field `{key}` must be a role between 1 and 100, got {raw}"
        ))
    })
}

/// One role on the wire (§7.1, §9).
///
/// `builtin` travels with the role so the UI can decline to offer a delete it
/// would only be refused for — admin and public are what "administer" and
/// "anonymous caller" mean, and neither is an installation's to remove.
pub fn role_json(role: &RoleDef) -> Json {
    json!({
        "role": role.role,
        "name": role.name,
        "description": role.description,
        "builtin": role.is_builtin(),
    })
}

pub fn str_field<'a>(obj: &'a Map<String, Json>, key: &str) -> Result<&'a str> {
    obj.get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| Error::invalid(format!("missing or non-string field `{key}`")))
}

/// A required, non-empty (after trimming) string field.
pub fn non_empty_str_field<'a>(obj: &'a Map<String, Json>, key: &str) -> Result<&'a str> {
    let value = str_field(obj, key)?;
    if value.trim().is_empty() {
        return Err(Error::invalid(format!("field `{key}` must not be empty")));
    }
    Ok(value)
}

/// An optional string field: the value if present and a string, else "". A
/// present-but-non-string value is treated as absent, which is what an optional
/// wire field means.
pub fn optional_str(obj: &Map<String, Json>, key: &str) -> String {
    obj.get(key)
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// An optional string field distinguishing "absent/null" (→ `None`) from a
/// present string (→ `Some`) — for a kind parameter like a folder or summary
/// field. A present non-string is an error.
pub fn optional_present_str(obj: &Map<String, Json>, key: &str) -> Option<String> {
    match obj.get(key) {
        Some(Json::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// An optional boolean field, defaulting to `false` when absent or null; a
/// present non-boolean is an error.
pub fn optional_bool(obj: &Map<String, Json>, key: &str) -> Result<bool> {
    match obj.get(key) {
        None | Some(Json::Null) => Ok(false),
        Some(Json::Bool(b)) => Ok(*b),
        Some(_) => Err(Error::invalid(format!("field `{key}` must be a boolean"))),
    }
}

/// An optional JSON-object field read as an [`Attrs`] bag, empty when absent or
/// null; a present non-object is an error.
pub fn attributes_field(obj: &Map<String, Json>) -> Result<Attrs> {
    match obj.get("attributes") {
        None | Some(Json::Null) => Ok(Attrs::new()),
        Some(Json::Object(map)) => Ok(map.clone()),
        Some(_) => Err(Error::invalid("`attributes` must be an object")),
    }
}

/// An optional array-of-strings field, empty when absent or null; a present value
/// that is not an array of strings is an error.
pub fn string_array_field(obj: &Map<String, Json>, key: &str) -> Result<Vec<String>> {
    match obj.get(key) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| Error::invalid(format!("`{key}` must be an array of strings")))
            })
            .collect(),
        Some(_) => Err(Error::invalid(format!(
            "`{key}` must be an array of strings"
        ))),
    }
}

/// A required integer field of an object body.
pub fn int_field(obj: &Map<String, Json>, key: &str) -> Result<i64> {
    obj.get(key)
        .and_then(Json::as_i64)
        .ok_or_else(|| Error::invalid(format!("missing or non-integer field `{key}`")))
}

/// A required boolean field of an object body.
pub fn bool_field(obj: &Map<String, Json>, key: &str) -> Result<bool> {
    obj.get(key)
        .and_then(Json::as_bool)
        .ok_or_else(|| Error::invalid(format!("missing or non-boolean field `{key}`")))
}

// --- remote MCP servers -------------------------------------------------------

/// A definition as the API returns it, its header values redacted.
pub fn server_json(def: &sc_mcp::McpServerDef) -> Json {
    with_headers(&def.redacted())
}

/// A definition for a **backup**, its header values included: the backup
/// carries them as it carries an LLM provider's API key.
pub fn backup_server_json(def: &sc_mcp::McpServerDef) -> Json {
    with_headers(def)
}

fn with_headers(def: &sc_mcp::McpServerDef) -> Json {
    json!({
        "id": def.id.0,
        "name": def.name,
        "description": def.description,
        "url": def.url,
        "transport": def.transport.as_str(),
        "headers": def
            .headers
            .iter()
            .map(|(name, value)| json!({ "name": name, "value": value }))
            .collect::<Vec<_>>(),
        "allow_private_network": def.allow_private_network,
        "timeout_secs": def.timeout_secs,
    })
}

/// Rebuild a definition from a create, update, test or restore body, the
/// sentinel in a header value resolved against `stored`.
pub fn server_from_body(
    id: sc_mcp::McpServerDefId,
    body: &Json,
    stored: &BTreeMap<String, String>,
) -> Result<sc_mcp::McpServerDef> {
    let obj = require_object(body)?;
    let text = |key: &str| obj.get(key).and_then(Json::as_str).unwrap_or("").to_owned();
    let mut headers = BTreeMap::new();
    for header in obj
        .get("headers")
        .and_then(Json::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let name = header
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            continue;
        }
        let value = header.get("value").and_then(Json::as_str).unwrap_or("");
        if headers.insert(name.to_owned(), value.to_owned()).is_some() {
            return Err(Error::invalid(format!("header `{name}` is given twice")));
        }
    }
    Ok(sc_mcp::McpServerDef {
        id,
        name: text("name").trim().to_owned(),
        description: text("description"),
        url: text("url").trim().to_owned(),
        transport: sc_mcp::McpTransport::parse(&text("transport"))?,
        headers: sc_mcp::merge_headers(stored, &headers)?,
        allow_private_network: obj
            .get("allow_private_network")
            .and_then(Json::as_bool)
            .unwrap_or(false),
        timeout_secs: obj.get("timeout_secs").and_then(Json::as_u64).unwrap_or(0),
        attributes: Map::new(),
    })
}

// --- Analytics datasets and workspaces -----------------------------------------

/// A definition from `createDataset`'s or `updateDataset`'s body, under `id`.
pub fn dataset_def_from_input(
    body: &Json,
    id: sc_dataset::DatasetId,
) -> Result<sc_dataset::DatasetDef> {
    let base: sc_dataset::Base = serde_json::from_value(
        body.get("base")
            .cloned()
            .ok_or_else(|| Error::invalid("`base` is required"))?,
    )
    .map_err(|e| Error::invalid(format!("`base` is not a table or a dataset: {e}")))?;
    let operations: Vec<sc_dataset::Operation> = match body.get("operations") {
        None | Some(Json::Null) => Vec::new(),
        Some(ops) => serde_json::from_value(ops.clone()).map_err(|e| {
            Error::invalid(format!("`operations` is not a list of operations: {e}"))
        })?,
    };
    Ok(sc_dataset::DatasetDef {
        id,
        name: body
            .get("name")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`name` is required"))?
            .trim()
            .to_owned(),
        description: body
            .get("description")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned(),
        base,
        operations,
    })
}

pub fn workspace_json(ws: &sc_analytics::Workspace) -> Json {
    json!({
        "id": ws.id.0,
        "name": ws.name,
        "kind": ws.kind.as_str(),
        "state": ws.state,
        "created_by": ws.created_by,
        "updated_at": ws.updated_at,
        "share_role": ws.share_role,
        "application": ws.application,
    })
}
