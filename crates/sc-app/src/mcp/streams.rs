//! Streams over MCP (TODO.md "Live updates" §2): `edit_streams`.
//!
//! A stream is instance-wide configuration — a provider and its settings, who
//! may observe it, whether it runs — and an application reaches one only by
//! listing it. This tool is the first half: it lists, creates, changes and
//! deletes streams for the whole instance. The second half, which streams an
//! application exposes, is `update_application`'s `streams` section, beside
//! its connected tables, because it is the same kind of step: the subset an
//! app may reach.
//!
//! ## One tool, not four
//!
//! Reading, saving and deleting streams are one tool rather than a
//! `describe_streams` / `save_stream` / `delete_stream` trio: every tool is one
//! more thing the model reads on every turn, and streams are a small corner of
//! building an app. A call with no changes is the listing, and a call with
//! changes answers with the listing as it is afterwards, so the model never
//! needs a second call to see what it did.
//!
//! ## Grants
//!
//! Creating a stream needs `allow_create`, changing one `allow_edit` (its
//! `min_role`, who may observe it, included: TODO-type-hardening H8.9 retired
//! the separate access grant) and deleting one `allow_drop`. Every grant and
//! every stream is checked before anything is written, so a refused item
//! changes nothing.
//!
//! ## Through the admin's own save
//!
//! With a running server the writes go through the admin API's `saveStream` and
//! `deleteStream`, which reload the stream supervisor, so a stream saved here
//! is flowing by the time the tool answers, exactly as one saved on the
//! Streams screen. Without one (a CLI command) the row is written directly and
//! runs from the next start. Either way, an application that exposes a changed
//! stream has its generated client rewritten, since the element type it is
//! typed from may have changed.

use sc_types::ArgsExt;
use std::sync::Arc;

use sc_api::mcp::{
    AdminTool, Area, GRANT_CREATE, GRANT_DROP, GRANT_EDIT, ToolContext, arguments, optional_role,
    require_grant, setting_json,
};
use sc_api::schema_edit::Grants;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_stream::{Stream, StreamRegistry};
use serde_json::{Map, Value as Json, json};

use crate::{Application, catalog_stream_registry, list_applications};

/// Lists, creates, changes and deletes the instance's streams.
pub const TOOL_EDIT_STREAMS: &str = "edit_streams";

const ARG_SAVE: &str = "save";
const ARG_DELETE: &str = "delete";
const ARG_NAME: &str = "name";
const ARG_PROVIDER: &str = "provider";
const ARG_DESCRIPTION: &str = "description";
const ARG_CONFIGURATION: &str = "configuration";
const ARG_MIN_ROLE: &str = "min_role";
const ARG_ENABLED: &str = "enabled";

/// The tool.
pub fn stream_tools() -> Vec<Arc<dyn AdminTool>> {
    vec![Arc::new(EditStreams)]
}

struct EditStreams;

#[async_trait::async_trait]
impl AdminTool for EditStreams {
    fn name(&self) -> &'static str {
        TOOL_EDIT_STREAMS
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Applications)
    }

    fn description(&self, catalog: &Catalog, grants: &Grants) -> String {
        edit_description(&catalog_stream_registry(catalog), grants)
    }

    fn parameters(&self) -> Json {
        edit_parameters()
    }

    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
        edit_streams(ctx, grants, args).await
    }
}

fn edit_description(registry: &StreamRegistry, grants: &Grants) -> String {
    let providers = registry
        .kinds()
        .iter()
        .map(|k| format!("`{}` ({})", k.name, k.description))
        .collect::<Vec<_>>()
        .join("; ");
    let mut refused = Vec::new();
    if !grants.create {
        refused.push("create a stream (`allow_create`)");
    }
    if !grants.edit {
        refused.push("change one (`allow_edit`)");
    }
    if !grants.drop {
        refused.push("delete one (`allow_drop`)");
    }
    let refused = match refused.is_empty() {
        true => String::new(),
        false => format!(" You may **not** {}.", refused.join(", ")),
    };
    format!(
        "List, create, change or delete the instance's **streams**: what an \
         application's pages subscribe to over its live socket — live row changes, \
         collaborative documents with presence, toasts and progress. Call it with no \
         arguments to list the streams, the applications exposing each, and every \
         provider's settings; every call answers with that listing as it is \
         afterwards.\n\n\
         - `{ARG_SAVE}` — a list of streams, each by `{ARG_NAME}` (an identifier). A \
         name that exists is changed: only the keys you give change, and \
         `{ARG_CONFIGURATION}` is merged key by key (null removes a key). A new one \
         needs `{ARG_PROVIDER}`. `{ARG_MIN_ROLE}` is who may subscribe through an \
         application (1 admin … 100 public; none means admin-only), and topic and \
         row checks narrow it further. `{ARG_ENABLED}` false stops it without \
         deleting it.\n\
         - `{ARG_DELETE}` — names to delete. Refused while a trigger listens to the \
         stream or an application exposes it.\n\n\
         The providers: {providers}. A `table_changes` or `document` stream's table \
         must exist (`edit_schema`; a document's field is of type `collab_doc`), and \
         a page reaches a stream only once its application exposes it: \
         `update_application` with `streams: {{ add: [...] }}`, which also needs \
         the stream's table connected. All items are checked before any is saved, \
         so a refused one changes nothing.{refused}"
    )
}

fn edit_parameters() -> Json {
    json!({
        "type": "object",
        "properties": {
            ARG_SAVE: {
                "type": "array",
                "description": "Streams to create or change.",
                "items": {
                    "type": "object",
                    "properties": {
                        ARG_NAME: {
                            "type": "string",
                            "description": "The stream's name: letters, digits and `_`.",
                        },
                        ARG_PROVIDER: {
                            "type": "string",
                            "description": "The provider; required for a new stream.",
                        },
                        ARG_DESCRIPTION: { "type": "string" },
                        ARG_CONFIGURATION: {
                            "type": "object",
                            "description":
                                "The provider's settings, by name. Merged into an \
                                 existing stream's; null removes a setting.",
                        },
                        ARG_MIN_ROLE: {
                            "type": ["integer", "null"],
                            "description":
                                "Who may subscribe: 1 (admin) to 100 (public); null is admin-only.",
                        },
                        ARG_ENABLED: { "type": "boolean" },
                    },
                    "required": [ARG_NAME],
                    "additionalProperties": false,
                },
            },
            ARG_DELETE: {
                "type": "array",
                "description": "Names of streams to delete.",
                "items": { "type": "string" },
            },
        },
        "additionalProperties": false,
    })
}

/// One `save` item, resolved against what is stored: the stream to write and
/// whether it is new.
struct Planned {
    stream: Stream,
    created: bool,
}

async fn edit_streams(ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
    let args = arguments(args, &[ARG_SAVE, ARG_DELETE])?;
    let registry = catalog_stream_registry(ctx.catalog);
    let saves = items(&args, ARG_SAVE)?;
    let deletes = delete_names(&args)?;
    if saves.is_empty() && deletes.is_empty() {
        let mut out = listing(ctx.catalog, &registry).await?;
        out["providers"] = providers_json(&registry);
        return Ok(out);
    }
    let has_table = ctx.catalog.get(sc_stream::STREAMS_TABLE)?.is_some();
    let apps = list_applications(ctx.catalog).await?;

    // Plan and check everything before writing anything.
    let mut planned: Vec<Planned> = Vec::new();
    for item in &saves {
        let plan = plan_save(ctx.catalog, has_table, grants, item).await?;
        if planned.iter().any(|p| p.stream.name == plan.stream.name)
            || deletes.contains(&plan.stream.name)
        {
            return Err(Error::invalid(format!(
                "stream `{}` is named twice in one call; nothing was changed",
                plan.stream.name
            )));
        }
        sc_stream::validate_stream(ctx.catalog, &registry, &plan.stream)
            .await
            .map_err(|e| Error::invalid(format!("{e}. Nothing was changed.")))?;
        planned.push(plan);
    }
    let mut doomed = Vec::new();
    for name in &deletes {
        require_grant(grants.drop, "delete a stream", GRANT_DROP)?;
        let stream = match has_table {
            true => sc_stream::load_stream_by_name(ctx.catalog, name).await?,
            false => None,
        }
        .ok_or_else(|| unknown_stream(name))?;
        let mut referents = trigger_referents(ctx.catalog, name).await?;
        referents.extend(
            exposed_by(&apps, name)
                .iter()
                .map(|app| format!("application `{app}`")),
        );
        if !referents.is_empty() {
            return Err(Error::invalid(format!(
                "stream `{name}` is still used by {}; remove those references first \
                 (`update_application` with `streams: {{ remove: [\"{name}\"] }}` for an \
                 application). Nothing was changed.",
                referents.join(", ")
            )));
        }
        doomed.push(stream);
    }

    let mut notes = Vec::new();
    let host = ctx.catalog.admin_host();
    let user = ctx.require_admin("streams")?;
    for plan in &planned {
        match &host {
            Some(host) => {
                host.save_stream(user, save_body(plan)).await?;
            }
            None => sc_stream::save_stream(ctx.catalog, &registry, &plan.stream).await?,
        }
    }
    for stream in &doomed {
        match &host {
            Some(host) => {
                host.delete_stream(user, stream.id.0).await?;
            }
            None => {
                sc_stream::delete_stream(ctx.catalog, stream.id, &[]).await?;
            }
        }
    }
    if host.is_none() {
        notes.push(format!(
            "saved; {} is not connected to a running server, so the streams run from \
             the server's next start",
            ctx.actor
        ));
    }
    // An exposing app's client is typed from the stream's element type, which a
    // changed configuration may have changed.
    for app in &apps {
        if planned
            .iter()
            .any(|p| app.streams.iter().any(|s| s.0 == p.stream.name))
            && let Err(e) = crate::emit_app_client(ctx.catalog, app, ctx.triggers).await
        {
            notes.push(format!(
                "the generated client of `{}` could not be rewritten: {e}",
                app.subdomain
            ));
        }
    }

    let mut out = listing(ctx.catalog, &registry).await?;
    out["created"] = json!(
        planned
            .iter()
            .filter(|p| p.created)
            .map(|p| &p.stream.name)
            .collect::<Vec<_>>()
    );
    out["changed"] = json!(
        planned
            .iter()
            .filter(|p| !p.created)
            .map(|p| &p.stream.name)
            .collect::<Vec<_>>()
    );
    out["deleted"] = json!(doomed.iter().map(|s| &s.name).collect::<Vec<_>>());
    if !notes.is_empty() {
        out["notes"] = json!(notes);
    }
    Ok(out)
}

/// One `save` item merged onto the stored stream of its name, or a new one,
/// with its grants checked.
async fn plan_save(
    catalog: &Catalog,
    has_table: bool,
    grants: &Grants,
    item: &Map<String, Json>,
) -> Result<Planned> {
    let name = item.required_string(ARG_NAME)?;
    let stored = match has_table {
        true => sc_stream::load_stream_by_name(catalog, &name).await?,
        false => None,
    };
    let provider = item
        .optional_string(ARG_PROVIDER)?
        .map(|p| p.trim().to_owned());
    let created = stored.is_none();
    let mut stream = match stored {
        Some(stored) => {
            require_grant(grants.edit, "change a stream", GRANT_EDIT)?;
            stored
        }
        None => {
            require_grant(grants.create, "create a stream", GRANT_CREATE)?;
            let provider = provider.clone().ok_or_else(|| {
                Error::invalid(format!(
                    "there is no stream `{name}`; give `{ARG_PROVIDER}` to create it. \
                     Nothing was changed."
                ))
            })?;
            Stream::new(name.clone(), provider)
        }
    };
    // A different provider takes different settings: start them afresh.
    if let Some(provider) = provider
        && provider != stream.provider
    {
        stream.provider = provider;
        stream.configuration.clear();
    }
    if let Some(description) = item.optional_string(ARG_DESCRIPTION)? {
        stream.description = description;
    }
    match item.get(ARG_CONFIGURATION) {
        None | Some(Json::Null) => {}
        Some(Json::Object(settings)) => {
            for (key, value) in settings {
                match value {
                    Json::Null => stream.configuration.remove(key),
                    value => stream.configuration.insert(key.clone(), value.clone()),
                };
            }
        }
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{ARG_CONFIGURATION}` of stream `{name}` should be an object of settings, \
                 got {other}"
            )));
        }
    }
    if item.contains_key(ARG_MIN_ROLE) {
        let role = optional_role(item, ARG_MIN_ROLE)?;
        if role != stream.min_role {
            stream.min_role = role;
        }
    }
    if let Some(enabled) = item.optional_bool(ARG_ENABLED)? {
        stream.set_enabled(enabled);
    }
    Ok(Planned { stream, created })
}

/// The admin API's `saveStream` body for `plan`: the whole definition, with the
/// id saying whether it replaces a row.
fn save_body(plan: &Planned) -> Json {
    let stream = &plan.stream;
    let mut body = json!({
        "name": stream.name,
        "provider": stream.provider,
        "description": stream.description,
        "configuration": Json::Object(stream.configuration.clone()),
        "min_role": stream.min_role,
        "attributes": Json::Object(stream.attributes.clone()),
        "enabled": stream.is_enabled(),
    });
    if !plan.created {
        body["id"] = json!(stream.id.0.to_string());
    }
    body
}

/// Every stored stream as the model reads it, under `streams`.
async fn listing(catalog: &Catalog, registry: &StreamRegistry) -> Result<Json> {
    let stored = match catalog.get(sc_stream::STREAMS_TABLE)?.is_some() {
        true => sc_stream::list_streams(catalog).await?,
        false => Vec::new(),
    };
    let apps = list_applications(catalog).await?;
    Ok(json!({
        "streams": stored
            .iter()
            .map(|s| stream_json(registry, &apps, s))
            .collect::<Vec<_>>(),
    }))
}

fn stream_json(registry: &StreamRegistry, apps: &[Application], stream: &Stream) -> Json {
    let redacted = sc_stream::redacted_stream(registry, stream);
    let mut out = json!({
        "name": stream.name,
        "description": stream.description,
        "provider": stream.provider,
        "configuration": Json::Object(redacted.configuration),
        "min_role": stream.min_role,
        "enabled": stream.is_enabled(),
        "exposed_by": exposed_by(apps, &stream.name),
    });
    match registry.get(stream.provider.trim()) {
        Some(provider) => match provider.topic_spec(&stream.configuration) {
            Ok(topics) => out["topics"] = serde_json::to_value(topics).unwrap_or(Json::Null),
            Err(e) => out["error"] = json!(e.to_string()),
        },
        None => {
            out["error"] = json!(format!(
                "no stream provider named `{}` is registered",
                stream.provider.trim()
            ))
        }
    }
    out
}

fn providers_json(registry: &StreamRegistry) -> Json {
    json!(
        registry
            .kinds()
            .iter()
            .map(|k| json!({
                "name": k.name,
                "description": k.description,
                "settings": k.config_spec.iter().map(setting_json).collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>()
    )
}

/// The subdomains of the applications exposing the stream `name`.
fn exposed_by(apps: &[Application], name: &str) -> Vec<String> {
    apps.iter()
        .filter(|app| app.streams.iter().any(|s| s.0 == name))
        .map(|app| app.subdomain.clone())
        .collect()
}

/// The triggers listening to the stream `name`, phrased as `delete_stream`
/// refuses with.
async fn trigger_referents(catalog: &Catalog, name: &str) -> Result<Vec<String>> {
    Ok(sc_action::list_triggers(catalog)
        .await?
        .into_iter()
        .filter(|t| {
            t.when == sc_action::EventKind::Stream
                && t.channel.as_deref().map(str::trim) == Some(name)
        })
        .map(|t| sc_stream::trigger_referent(&t.name))
        .collect())
}

fn unknown_stream(name: &str) -> Error {
    Error::invalid(format!(
        "there is no stream `{name}`; call `{TOOL_EDIT_STREAMS}` with no arguments to list \
         them. Nothing was changed."
    ))
}

/// The `save` items, each an object.
fn items(args: &Map<String, Json>, key: &str) -> Result<Vec<Map<String, Json>>> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(items)) => items
            .iter()
            .map(|item| {
                arguments(
                    item,
                    &[
                        ARG_NAME,
                        ARG_PROVIDER,
                        ARG_DESCRIPTION,
                        ARG_CONFIGURATION,
                        ARG_MIN_ROLE,
                        ARG_ENABLED,
                    ],
                )
                .map_err(|e| Error::invalid(format!("`{key}`: {e}")))
            })
            .collect(),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` should be a list of streams, got {other}"
        ))),
    }
}

fn delete_names(args: &Map<String, Json>) -> Result<Vec<String>> {
    match args.get(ARG_DELETE) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        Error::invalid(format!("`{ARG_DELETE}` should be a list of stream names"))
                    })
            })
            .collect(),
        Some(other) => Err(Error::invalid(format!(
            "`{ARG_DELETE}` should be a list of stream names, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_names_the_providers_and_what_the_grants_refuse() {
        let registry = sc_stream::builtin_registry().unwrap();
        let all = edit_description(&registry, &Grants::all());
        assert!(all.contains("`document`"), "{all}");
        assert!(all.contains("`table_changes`"), "{all}");
        assert!(!all.contains("You may **not**"), "{all}");
        let none = edit_description(&registry, &Grants::none());
        assert!(!none.contains("allow_access_changes"), "retired: {none}");
        assert!(none.contains("delete one (`allow_drop`)"), "{none}");
    }
}
