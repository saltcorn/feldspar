//! The registry editor (APP_SYNC_PROPOSAL §7.6): the instance tree one file at
//! a time, and a save as a one-entity deploy against live (`sc_sync::registry`).

use std::collections::BTreeSet;

use super::*;

/// Register this domain's admin handlers on `reg`.
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();
    let apps = state.apps.clone();

    reg.handle(sc_api::handler_ids::LIST_REGISTRY, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let export = deployer(&catalog, &apps, None)?.instance_export().await?;
                Ok(HandlerResponse::ok(json!({
                    "entries": sc_sync::registry::list(&export),
                })))
            }
        }
    });

    reg.handle(sc_api::handler_ids::GET_REGISTRY_ENTITY, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let path = ctx
                    .query_get("path")
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| Error::invalid("`path` names the registry entry to open"))?;
                let export = deployer(&catalog, &apps, None)?.instance_export().await?;
                let entity = sc_sync::registry::get(&export, path).ok_or_else(|| {
                    Error::not_found(format!("there is no `{path}` in the registry"))
                })?;
                let schema = editor_schema(&entity.entry);
                let mut out = serde_json::to_value(entity)
                    .map_err(|e| Error::msg(format!("the registry entry `{path}`: {e}")))?;
                out["schema"] = schema.unwrap_or(Json::Null);
                // With a link, the base's and the tip's versions, for the
                // diff tabs and a merge. The registry works without a link
                // (D7), so a link that cannot be read leaves them out rather
                // than refusing the entry.
                let state = HandlerState {
                    catalog: catalog.clone(),
                    apps: apps.clone(),
                };
                let sides = super::source_control::source(&state, None)?
                    .sides(path)
                    .await
                    .ok()
                    .flatten();
                out["sync"] = serde_json::to_value(sides)
                    .map_err(|e| Error::msg(format!("the registry entry `{path}`: {e}")))?;
                Ok(HandlerResponse::ok(out))
            }
        }
    });

    reg.handle(sc_api::handler_ids::PLAN_REGISTRY_SAVE, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let edit = edit_from_body(&ctx.body)?;
                let planned = deployer(&catalog, &apps, Some(ctx.who.editor()))?
                    .plan_edit(&edit)
                    .await?;
                let mut out = serde_json::to_value(&planned.plan)
                    .map_err(|e| Error::msg(format!("the plan: {e}")))?;
                out["before"] = planned.before.unwrap_or(Json::Null);
                out["value"] = planned.value.unwrap_or(Json::Null);
                Ok(HandlerResponse::ok(out))
            }
        }
    });

    reg.handle(sc_api::handler_ids::REGISTRY_SAVE, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let edit = edit_from_body(&ctx.body)?;
                let obj = require_object(&ctx.body)?;
                let confirm = sc_sync::Confirm {
                    plan_digest: obj.non_empty_str_field("plan_digest")?.to_owned(),
                    ticked: ticked(obj.get("ticked"))?,
                    allow_destructive: false,
                    full_backup: false,
                };
                let by = sc_sync::EditBy {
                    actor: ctx
                        .who
                        .user()
                        .and_then(|u| u.get(COL_EMAIL))
                        .and_then(sc_query::Value::as_text)
                        .map(str::to_owned),
                    source: sc_sync::Source::Ui,
                    // The policy stored with the sync link, so a save that
                    // drops data is refused under `destructive: never` as a
                    // deploy is.
                    policy: sc_sync::load_link(&catalog)
                        .await?
                        .map(|l| l.policy)
                        .unwrap_or_default(),
                };
                let receipt = deployer(&catalog, &apps, Some(ctx.who.editor()))?
                    .edit(&edit, &confirm, &by)
                    .await?;
                // A save can change what an application's API is projected
                // from, as any schema or trigger edit through this API can.
                reproject_apps(&apps);
                Ok(HandlerResponse::ok(
                    serde_json::to_value(receipt)
                        .map_err(|e| Error::msg(format!("the receipt: {e}")))?,
                ))
            }
        }
    });
}

/// A deployer over the running server, as `editor` (or as an admin read, for
/// the reads, which change nothing).
fn deployer<'a>(
    catalog: &'a Catalog,
    apps: &'a Arc<AppMounts>,
    editor: Option<sc_catalog::Editor>,
) -> Result<sc_sync::Deployer<'a>> {
    Ok(sc_sync::Deployer::new(sc_sync::Executor {
        catalog,
        host: apps.as_ref(),
        editor: editor.unwrap_or(sc_catalog::Editor::Delegated("registry read")),
        data_dir: sc_files::data_dir()?,
    }))
}

/// The save a body asks for: `{ path, value, version }`.
fn edit_from_body(body: &Json) -> Result<sc_sync::Edit> {
    let obj = require_object(body)?;
    let text = |key: &str| -> Result<Option<String>> {
        match obj.get(key) {
            None | Some(Json::Null) => Ok(None),
            Some(Json::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(Error::invalid(format!("`{key}` must be text"))),
        }
    };
    Ok(sc_sync::Edit {
        path: obj.non_empty_str_field("path")?.to_owned(),
        value: obj.get("value").filter(|v| !v.is_null()).cloned(),
        version: text("version")?,
    })
}

fn ticked(value: Option<&Json>) -> Result<BTreeSet<String>> {
    value
        .and_then(Json::as_array)
        .ok_or_else(|| Error::invalid("`ticked` must list the operations to apply"))?
        .iter()
        .map(|id| {
            id.as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::invalid("`ticked` must list operation ids"))
        })
        .collect()
}

/// The JSON Schema the editor checks an entry against as it is typed, where
/// the API's types describe it.
///
/// A settings section's is exact: its declared keys and each one's type, and
/// no other key (a save refuses one). A kind an admin endpoint takes whole is
/// described by that endpoint's input, **opened**: no key required and none
/// refused, since the registry's JSON is the codec's (`*_json`) and carries
/// what the form does not send — an id, a derived list. It checks the types of
/// what it names, which is what catches a typo'd value.
fn editor_schema(entry: &sc_sync::RegistryEntry) -> Option<Json> {
    if entry.kind == sc_sync::Kind::Setting {
        return settings_schema(&entry.name);
    }
    let endpoint = match entry.kind {
        sc_sync::Kind::Application => "updateApplication",
        sc_sync::Kind::Trigger => "updateTrigger",
        sc_sync::Kind::Agent => "updateAgent",
        sc_sync::Kind::FileStore => "updateFileStore",
        sc_sync::Kind::Stream => "saveStream",
        sc_sync::Kind::View => "saveView",
        sc_sync::Kind::Page => "savePage",
        _ => return None,
    };
    let set = sc_api::admin_endpoints();
    let input = &set.iter().find(|e| e.name == endpoint)?.input;
    if input.is_empty() {
        return None;
    }
    let mut schema = sc_api::mcp::json_schema(input);
    open_schema(&mut schema);
    Some(schema)
}

/// A settings section's keys, each typed by its declaration; a secret is a
/// string (the sentinel, or a new value).
fn settings_schema(section: &str) -> Option<Json> {
    let fields = sc_sync::export::settings_section_fields(section)?;
    let mut properties = Map::new();
    for field in fields {
        let schema = match &field.base.type_ {
            sc_types::TypeRef::Basic(basic) => {
                sc_api::mcp::scalar_schema(sc_api::ValueType::from_basic(basic))
            }
            _ => json!({}),
        };
        properties.insert(field.name().to_owned(), schema);
    }
    Some(sc_api::mcp::object_schema(properties, Vec::new()))
}

/// `schema` with nothing required and every object open.
fn open_schema(schema: &mut Json) {
    match schema {
        Json::Object(map) => {
            map.remove("required");
            if map.get("additionalProperties") == Some(&Json::Bool(false)) {
                map.remove("additionalProperties");
            }
            map.values_mut().for_each(open_schema);
        }
        Json::Array(items) => items.iter_mut().for_each(open_schema),
        _ => {}
    }
}
