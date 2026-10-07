//! Changing an existing application (§13.2, §13.6): `update_application`.
//!
//! One tool with a section per part of the record an agent may change, rather
//! than a tool per part: every tool is one more thing the model reads and
//! chooses between on every turn, and these all have the same shape — name the
//! application, say what changes, save. The sections are:
//!
//! - **`tables`**, the connected tables: an application reaches only those, and
//!   its generated client is typed for them alone, so this is the step between
//!   `edit_schema` and writing code.
//! - **`static_dirs`**: a file store's folder served on a URL path of the app.
//! - **`csp`**: single directives of the app's Content-Security-Policy.
//!
//! ## A grant per section, checked before anything changes
//!
//! As `edit_schema` checks a grant per operation, this checks one per section,
//! and a call naming a section it may not touch changes nothing at all. The
//! connected tables are an edit: connecting a table does not open its rows,
//! whose own roles still decide who reads them. A static directory is an edit
//! too, because a mount is not a grant — every file it serves goes through the
//! same access check as the file manager, as the request's user (the router's
//! `serve_static_dir`). A CSP **is** a security boundary — it decides what a
//! page may load and who may frame it — so the `csp` section needs
//! `allow_access_changes`, the grant that also guards a table's roles and a
//! query's `min_role`.
//!
//! ## Through the admin's own update
//!
//! A mounted application is served from the record it was mounted with, so a
//! write of the record alone would change nothing a browser sees until a
//! restart. This tool therefore saves the way the admin's Save button does:
//! `listApplications` for the stored record, the changed keys written into it,
//! `updateApplication` with the result — which validates, saves, rewrites the
//! generated client and refreshes the mount, refused in the same words as the
//! form. A context with no server (a CLI command) saves the record and rewrites
//! the client itself, and says the change is served from the next start.

use std::sync::Arc;

use sc_api::mcp::{AdminTool, Area, ToolContext, arguments, optional_string, require_grant};
use sc_api::schema_edit::{GRANT_ACCESS_CHANGES, GRANT_EDIT, Grants};
use sc_catalog::{AdminCall, Catalog, FileStoreId, TableId};
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};

use super::{TOOL_DESCRIBE_APPS, load_app, required_str};
use crate::{Application, CspPolicy, StaticDir};

/// Changes an application's connected tables, static directories or CSP.
pub const TOOL_UPDATE_APP: &str = "update_application";

/// The tool.
pub fn update_tools() -> Vec<Arc<dyn AdminTool>> {
    vec![Arc::new(UpdateApp)]
}

const ARG_APPLICATION: &str = "application";
const ARG_TABLES: &str = "tables";
const ARG_STATIC_DIRS: &str = "static_dirs";
const ARG_CSP: &str = "csp";
const ARG_ADD: &str = "add";
const ARG_REMOVE: &str = "remove";
const ARG_SET: &str = "set";
const ARG_MOUNT: &str = "mount";
const ARG_STORE: &str = "file_store";
const ARG_PATH: &str = "path";

struct UpdateApp;

#[async_trait::async_trait]
impl AdminTool for UpdateApp {
    fn name(&self) -> &'static str {
        TOOL_UPDATE_APP
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Applications)
    }

    fn description(&self, _catalog: &Catalog, grants: &Grants) -> String {
        update_description(grants)
    }

    fn parameters(&self) -> Json {
        update_parameters()
    }

    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
        update_app(ctx, grants, args).await
    }
}

fn update_description(grants: &Grants) -> String {
    let edit = match grants.edit {
        true => "",
        false => " You may **not** change `tables` or `static_dirs`: they need `allow_edit`.",
    };
    let csp = match grants.access_changes {
        true => {
            " A CSP is a security boundary, so say plainly what you are about to \
                 widen, and why, before you do it."
        }
        false => {
            " You may **not** change `csp`: it needs `allow_access_changes`. Tell \
                  the person which directive the app needs instead."
        }
    };
    format!(
        "Change an application: the tables it serves, the store folders it serves \
         as files, or its Content-Security-Policy. Give only the sections you are \
         changing; one call may change several, and is saved as one, so a refused \
         section changes nothing.\n\n\
         - `{ARG_TABLES}` — its **connected tables**. An application reaches only \
         these, and its typed client (`src/feldspar/`) is regenerated for them, so \
         connect a table created with `edit_schema` **before** writing code that \
         uses it. `{ARG_ADD}` and `{ARG_REMOVE}` change the list; `{ARG_SET}` \
         replaces it. Connecting a table does not make its rows public: its own \
         role rules still decide who reads and writes.\n\
         - `{ARG_STATIC_DIRS}` — a store folder served at a URL path: a request for \
         `<mount>/a/b.png` is answered with `<path>/a/b.png` from the store, and a \
         folder with its `index.html`. `{ARG_ADD}` lists `{{ {ARG_MOUNT}, {ARG_STORE}, \
         {ARG_PATH} }}` (a mount in use is replaced; a store the app was not \
         connected to is connected); `{ARG_REMOVE}` lists mounts. A mount must not \
         be at or under one of the app's API mounts. Each file is still served \
         only to someone its store's and its own access rules let read it.\n\
         - `{ARG_CSP}` — maps a directive to its whole new source list, or to \
         `null` to remove it; directives not named are kept. The usual reasons: \
         `frame-ancestors` naming another app's origin (`'self' \
         https://admin.example.com`) so it may show this one in an iframe; \
         `style-src 'self' 'unsafe-inline'` for `style` attributes written into \
         markup; `img-src` or `connect-src` naming an outside host. Widen only \
         what the code needs.\n\n\
         `{TOOL_DESCRIBE_APPS}` shows what each application has now. The running \
         app serves the change at once; a bundle written against tables it did \
         not have still needs a rebuild.{edit}{csp}"
    )
}

fn update_parameters() -> Json {
    let names = json!({ "type": "array", "items": { "type": "string" } });
    json!({
        "type": "object",
        "properties": {
            ARG_APPLICATION: {
                "type": "string",
                "description": "The application, by subdomain.",
            },
            ARG_TABLES: {
                "type": "object",
                "description": "Change the connected tables.",
                "properties": {
                    ARG_ADD: names.clone(),
                    ARG_REMOVE: names.clone(),
                    ARG_SET: names.clone(),
                },
                "additionalProperties": false,
            },
            ARG_STATIC_DIRS: {
                "type": "object",
                "description": "Change the store folders served as files.",
                "properties": {
                    ARG_ADD: {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                ARG_MOUNT: {
                                    "type": "string",
                                    "description": "The URL path within the app, e.g. `/media`.",
                                },
                                ARG_STORE: {
                                    "type": "string",
                                    "description": "The file store the folder is in.",
                                },
                                ARG_PATH: {
                                    "type": "string",
                                    "description": "The folder in the store; empty for its root.",
                                },
                            },
                            "required": [ARG_MOUNT, ARG_STORE],
                            "additionalProperties": false,
                        },
                    },
                    ARG_REMOVE: names,
                },
                "additionalProperties": false,
            },
            ARG_CSP: {
                "type": "object",
                "description":
                    "Directive name (`frame-ancestors`, `style-src`, …) → its sources, \
                     or null to remove the directive.",
                "additionalProperties": {
                    "type": ["array", "null"],
                    "items": { "type": "string" },
                },
            },
        },
        "required": [ARG_APPLICATION],
        "additionalProperties": false,
    })
}

async fn update_app(ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
    let args = arguments(
        args,
        &[ARG_APPLICATION, ARG_TABLES, ARG_STATIC_DIRS, ARG_CSP],
    )?;
    let tables = section(&args, ARG_TABLES, &[ARG_ADD, ARG_REMOVE, ARG_SET])?;
    let static_dirs = section(&args, ARG_STATIC_DIRS, &[ARG_ADD, ARG_REMOVE])?;
    let csp = match args.get(ARG_CSP) {
        None | Some(Json::Null) => None,
        Some(directives) => Some(directives),
    };
    if tables.is_none() && static_dirs.is_none() && csp.is_none() {
        return Err(Error::invalid(format!(
            "name at least one of `{ARG_TABLES}`, `{ARG_STATIC_DIRS}` and `{ARG_CSP}`; \
             nothing was changed"
        )));
    }
    // Every grant before any change: a refused section refuses the call.
    if tables.is_some() {
        require_grant(
            grants.edit,
            "change which tables an application serves",
            GRANT_EDIT,
        )?;
    }
    if static_dirs.is_some() {
        require_grant(
            grants.edit,
            "change which folders an application serves",
            GRANT_EDIT,
        )?;
    }
    if csp.is_some() {
        require_grant(
            grants.access_changes,
            "change an application's Content-Security-Policy",
            GRANT_ACCESS_CHANGES,
        )?;
    }

    let mut app = load_app(ctx, &args).await?;
    let mut out = json!({ "application": app.subdomain });
    let mut was = Map::new();
    if let Some(tables) = &tables {
        was.insert(ARG_TABLES.to_owned(), tables_json(&app));
        change_tables(ctx.catalog, &mut app, tables)?;
    }
    if let Some(dirs) = &static_dirs {
        was.insert(
            ARG_STATIC_DIRS.to_owned(),
            static_dirs_json(&app.static_dirs),
        );
        let connected = change_static_dirs(ctx.catalog, &mut app, dirs)?;
        if !connected.is_empty() {
            out["connected_file_stores"] = json!(connected);
        }
    }
    if let Some(directives) = csp {
        was.insert(ARG_CSP.to_owned(), csp_json(&app.csp));
        app.csp = changed_csp(&app.csp, directives)?;
    }

    let saved = save(ctx, &app, &mut out).await?;
    if tables.is_some() {
        out[ARG_TABLES] = tables_json(&saved);
    }
    if static_dirs.is_some() {
        out[ARG_STATIC_DIRS] = static_dirs_json(&saved.static_dirs);
    }
    if csp.is_some() {
        out[ARG_CSP] = csp_json(&saved.csp);
        out["csp_header"] = json!(saved.csp.header_value());
    }
    out["was"] = Json::Object(was);
    Ok(out)
}

/// A section's object with its keys checked, or `None` when it was not given.
fn section(
    args: &Map<String, Json>,
    key: &str,
    allowed: &[&str],
) -> Result<Option<Map<String, Json>>> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(value @ Json::Object(_)) => arguments(value, allowed)
            .map(Some)
            .map_err(|e| Error::invalid(format!("`{key}`: {e}"))),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` should be an object with {}, got {other}",
            allowed
                .iter()
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

// --- tables -------------------------------------------------------------------

fn change_tables(
    catalog: &Catalog,
    app: &mut Application,
    section: &Map<String, Json>,
) -> Result<()> {
    let mut tables: Vec<String> = match section.contains_key(ARG_SET) {
        true => names(section, ARG_SET)?,
        false => app.tables.iter().map(|t| t.0.clone()).collect(),
    };
    for add in names(section, ARG_ADD)? {
        if !tables.contains(&add) {
            tables.push(add);
        }
    }
    let remove = names(section, ARG_REMOVE)?;
    tables.retain(|t| !remove.contains(t));
    for table in &tables {
        if catalog.get(table)?.is_none() {
            return Err(Error::invalid(format!(
                "there is no table `{table}`; create it with `edit_schema` first. \
                 Nothing was changed."
            )));
        }
    }
    app.tables = tables.into_iter().map(TableId).collect();
    Ok(())
}

fn tables_json(app: &Application) -> Json {
    json!(app.tables.iter().map(|t| t.0.clone()).collect::<Vec<_>>())
}

// --- static_dirs --------------------------------------------------------------

/// Apply the section, returning the stores it connected.
fn change_static_dirs(
    catalog: &Catalog,
    app: &mut Application,
    section: &Map<String, Json>,
) -> Result<Vec<String>> {
    let remove: Vec<String> = names(section, ARG_REMOVE)?
        .iter()
        .map(|m| StaticDir::new(m.as_str(), FileStoreId(String::new()), "").mount)
        .collect();
    for mount in &remove {
        if !app.static_dirs.iter().any(|d| &d.mount == mount) {
            return Err(Error::invalid(format!(
                "application `{}` serves no static directory at `{mount}`; it serves {}. \
                 Nothing was changed.",
                app.subdomain,
                mount_list(&app.static_dirs)
            )));
        }
    }
    app.static_dirs.retain(|d| !remove.contains(&d.mount));

    let mut connected = Vec::new();
    for dir in additions(section)? {
        if catalog.file_store(&dir.store.0)?.is_none() {
            return Err(Error::invalid(format!(
                "there is no file store `{}`; the file stores are {}. Nothing was changed.",
                dir.store.0,
                quoted(&catalog.file_store_names()?)
            )));
        }
        if !app.file_stores.contains(&dir.store) {
            app.file_stores.push(dir.store.clone());
            connected.push(dir.store.0.clone());
        }
        app.static_dirs.retain(|d| d.mount != dir.mount);
        app.static_dirs.push(dir);
    }
    crate::validate_static_dirs(app)?;
    Ok(connected)
}

/// The `add` items, each normalised the way the record stores it.
fn additions(section: &Map<String, Json>) -> Result<Vec<StaticDir>> {
    let shape = format!("{{ {ARG_MOUNT}, {ARG_STORE}, {ARG_PATH} }}");
    let items = match section.get(ARG_ADD) {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(items)) => items,
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{ARG_STATIC_DIRS}.{ARG_ADD}` should be a list of {shape}, got {other}"
            )));
        }
    };
    items
        .iter()
        .map(|item| {
            if !item.is_object() {
                return Err(Error::invalid(format!(
                    "each of `{ARG_STATIC_DIRS}.{ARG_ADD}` should be {shape}, got {item}"
                )));
            }
            let obj = arguments(item, &[ARG_MOUNT, ARG_STORE, ARG_PATH])?;
            let path = optional_string(&obj, ARG_PATH)?
                .map(|p| p.trim().trim_matches('/').to_owned())
                .unwrap_or_default();
            Ok(StaticDir::new(
                required_str(&obj, ARG_MOUNT)?,
                FileStoreId(required_str(&obj, ARG_STORE)?),
                path,
            ))
        })
        .collect()
}

pub(super) fn static_dirs_json(dirs: &[StaticDir]) -> Json {
    json!(
        dirs.iter()
            .map(|d| json!({ "mount": d.mount, "file_store": d.store.0, "path": d.path }))
            .collect::<Vec<_>>()
    )
}

fn mount_list(dirs: &[StaticDir]) -> String {
    match dirs.is_empty() {
        true => "none".to_owned(),
        false => quoted(&dirs.iter().map(|d| d.mount.clone()).collect::<Vec<_>>()),
    }
}

// --- csp ----------------------------------------------------------------------

/// `csp` with the requested directives replaced or removed.
///
/// A directive name and its sources go into a response header verbatim, so
/// they are checked here rather than trusted: a `;` in a source would start a
/// directive of its own, and a newline would end the header.
fn changed_csp(csp: &CspPolicy, directives: &Json) -> Result<CspPolicy> {
    let Json::Object(directives) = directives else {
        return Err(Error::invalid(format!(
            "`{ARG_CSP}` should map a directive name to a list of sources or null"
        )));
    };
    if directives.is_empty() {
        return Err(Error::invalid(format!(
            "`{ARG_CSP}` names no directive; nothing was changed"
        )));
    }
    let mut csp = csp.clone();
    for (name, sources) in directives {
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
            return Err(Error::invalid(format!(
                "`{name}` is not a CSP directive name; nothing was changed"
            )));
        }
        match sources {
            Json::Null => {
                csp.directives.remove(&name);
            }
            Json::Array(items) => {
                let sources = items
                    .iter()
                    .map(|s| {
                        let s = s.as_str().map(str::trim).unwrap_or_default();
                        let valid = !s.is_empty()
                            && s.chars()
                                .all(|c| c.is_ascii_graphic() && c != ';' && c != ',');
                        match valid {
                            true => Ok(s.to_owned()),
                            false => Err(Error::invalid(format!(
                                "`{name}` has a source that is not one: {s:?}. A source \
                                 is one word such as `'self'` or `https://example.com`; \
                                 nothing was changed"
                            ))),
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                csp.directives.insert(name, sources);
            }
            other => {
                return Err(Error::invalid(format!(
                    "`{name}` should be a list of sources or null, got {other}"
                )));
            }
        }
    }
    Ok(csp)
}

pub(super) fn csp_json(csp: &CspPolicy) -> Json {
    Json::Object(
        csp.directives
            .iter()
            .map(|(name, sources)| (name.clone(), json!(sources)))
            .collect(),
    )
}

// --- shared -------------------------------------------------------------------

/// Save `app` through the server's own `updateApplication`, or — with no
/// server — save the record and rewrite the client here. Returns what was
/// stored; the mount's news goes into `out`.
async fn save(ctx: &ToolContext<'_>, app: &Application, out: &mut Json) -> Result<Application> {
    let Some(host) = ctx.catalog.admin_host() else {
        let saved = crate::save_application(ctx.catalog, app).await?;
        let mut notes = vec![format!(
            "saved; {} is not connected to a running server, so the change is served \
             from the server's next start",
            ctx.actor
        )];
        if let Err(e) = crate::emit_app_client(ctx.catalog, &saved, ctx.triggers).await {
            notes.push(format!("the generated client could not be rewritten: {e}"));
        }
        out["notes"] = json!(notes);
        return Ok(saved);
    };
    let user = ctx.user.map(|u| u.id);
    let listed = host
        .call_admin(AdminCall::new("listApplications", Json::Null).user(user))
        .await?;
    let id = app.id.to_string();
    let mut body = listed
        .as_array()
        .and_then(|apps| apps.iter().find(|a| a["id"] == json!(id)))
        .cloned()
        .ok_or_else(|| {
            Error::msg(format!(
                "application `{}` is stored but the server did not list it",
                app.subdomain
            ))
        })?;
    // Every key this tool can change, written whether or not this call changed
    // it: `app` was loaded fresh, so an unchanged key is written as it was.
    // The admin API names a static directory's store `store`.
    body["tables"] = tables_json(app);
    body["file_stores"] = json!(
        app.file_stores
            .iter()
            .map(|s| s.0.clone())
            .collect::<Vec<_>>()
    );
    body["static_dirs"] = json!(
        app.static_dirs
            .iter()
            .map(|d| json!({ "mount": d.mount, "store": d.store.0, "path": d.path }))
            .collect::<Vec<_>>()
    );
    body["csp"] = csp_json(&app.csp);
    let updated = host
        .call_admin(
            AdminCall::new("updateApplication", body)
                .param("id", id)
                .user(user),
        )
        .await?;
    for key in ["mounted", "mount_error"] {
        if let Some(value) = updated.get(key) {
            out[key] = value.clone();
        }
    }
    crate::load_application(ctx.catalog, app.id)
        .await?
        .ok_or_else(|| Error::msg(format!("application `{}` vanished", app.subdomain)))
}

/// An optional list of names, trimmed, empty ones dropped.
fn names(section: &Map<String, Json>, key: &str) -> Result<Vec<String>> {
    match section.get(key) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(|s| s.trim().to_owned())
                    .ok_or_else(|| Error::invalid(format!("`{key}` should be a list of names")))
            })
            .filter(|s| s.as_ref().map_or(true, |s| !s.is_empty()))
            .collect(),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` should be a list of names, got {other}"
        ))),
    }
}

fn quoted(names: &[String]) -> String {
    match names.is_empty() {
        true => "none yet".to_owned(),
        false => names
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directive_is_replaced_or_removed_and_the_rest_kept() {
        let csp = CspPolicy::strict()
            .directive("frame-ancestors", ["'none'"])
            .directive("img-src", ["'self'", "data:"]);
        let changed = changed_csp(
            &csp,
            &json!({
                "frame-ancestors": ["'self'", "https://verwaltung.example.com"],
                "img-src": null,
                "Style-Src": ["'self'", "'unsafe-inline'"],
            }),
        )
        .unwrap();
        assert_eq!(
            changed.header_value(),
            "default-src 'self'; frame-ancestors 'self' https://verwaltung.example.com; \
             style-src 'self' 'unsafe-inline'"
        );
    }

    #[test]
    fn a_source_cannot_smuggle_a_directive_or_a_header() {
        let csp = CspPolicy::strict();
        for bad in ["'self'; script-src *", "a\r\nX-Evil: 1", ""] {
            let err = changed_csp(&csp, &json!({ "img-src": [bad] }))
                .unwrap_err()
                .to_string();
            assert!(err.contains("nothing was changed"), "{err}");
        }
        assert!(changed_csp(&csp, &json!({ "img src": ["'self'"] })).is_err());
        assert!(changed_csp(&csp, &json!({})).is_err());
    }

    #[test]
    fn the_description_says_which_sections_the_grants_do_not_allow() {
        let all = update_description(&Grants::all());
        assert!(!all.contains("You may **not**"), "{all}");
        let none = update_description(&Grants::none());
        assert!(
            none.contains("**not** change `tables` or `static_dirs`: they need `allow_edit`"),
            "{none}"
        );
        assert!(
            none.contains("**not** change `csp`: it needs `allow_access_changes`"),
            "{none}"
        );
    }
}
