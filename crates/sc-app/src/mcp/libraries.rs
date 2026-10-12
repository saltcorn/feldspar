//! Code libraries over MCP (TODO F3.4, design "The code library"):
//! `describe_code_libraries`, `save_code_library` and `delete_code_library`.
//!
//! A library is named, shared JavaScript that code queries and triggers import
//! with `lib(name)`. These tools speak **names** — an application by subdomain,
//! a library by its name within that scope — because that is how an agent
//! refers to them and how `lib()` resolves them; the admin API's ids stay
//! behind the scenes.
//!
//! In the **applications** area: a library is code that runs inside the
//! application's queries (and the instance's triggers), so an agent limited to
//! the schema is not offered a way to write code. Saving needs `allow_edit` (create and
//! edit alike, since editing a shared library changes every importer and
//! adding one is harmless until something imports it) and deleting needs
//! `allow_drop`.

use std::sync::Arc;

use sc_api::mcp::{
    AdminTool, Area, GRANT_DROP, GRANT_EDIT, ToolContext, arguments, optional_string, require_grant,
};
use sc_api::schema_edit::Grants;
use sc_catalog::{Catalog, CodeLibrary};
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use super::{ARG_APPLICATION, unknown_application};

/// Lists the code libraries, their code and what imports each.
pub const TOOL_DESCRIBE_CODE_LIBRARIES: &str = "describe_code_libraries";
/// Creates a library, or edits the one already holding the name in its scope.
pub const TOOL_SAVE_CODE_LIBRARY: &str = "save_code_library";
/// Deletes one.
pub const TOOL_DELETE_CODE_LIBRARY: &str = "delete_code_library";

const ARG_NAME: &str = "name";
const ARG_DESCRIPTION: &str = "description";
const ARG_CODE: &str = "code";

/// The three tools.
pub fn library_tools() -> Vec<Arc<dyn AdminTool>> {
    vec![
        Arc::new(DescribeLibraries),
        Arc::new(SaveLibrary),
        Arc::new(DeleteLibrary),
    ]
}

struct DescribeLibraries;

#[async_trait::async_trait]
impl AdminTool for DescribeLibraries {
    fn name(&self) -> &'static str {
        TOOL_DESCRIBE_CODE_LIBRARIES
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Applications)
    }

    fn description(&self, _catalog: &Catalog, _grants: &Grants) -> String {
        format!(
            "List the code libraries: named JavaScript shared by code queries and \
             triggers, imported with `const {{ f }} = lib(\"name\")`. Each comes with \
             its code, its scope (`application` — a subdomain — or null for \
             instance-wide) and `used_by`, the queries and triggers whose code \
             imports it by name. Read this before `{TOOL_SAVE_CODE_LIBRARY}` so an \
             edit does not break an importer, and before writing the same helper \
             into a second query."
        )
    }

    fn parameters(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                ARG_APPLICATION: {
                    "type": "string",
                    "description": "Only this application's libraries and the \
                        instance-wide ones it can see, by subdomain. Omit for all."
                }
            },
            "additionalProperties": false
        })
    }

    async fn call(&self, ctx: &ToolContext<'_>, _grants: &Grants, args: &Json) -> Result<Json> {
        let args = arguments(args, &[ARG_APPLICATION])?;
        let mut libraries = crate::describe_code_libraries(ctx.catalog).await?;
        if let Some(subdomain) = optional_string(&args, ARG_APPLICATION)? {
            application_id(ctx, &subdomain).await?;
            libraries
                .retain(|l| l["application"].is_null() || l["application"] == json!(subdomain));
        }
        Ok(json!({
            "libraries": libraries,
            "how_to_import": "const { name } = lib(\"library\"); — the application's own \
                library of that name wins over the instance-wide one; a trigger sees \
                instance-wide libraries only.",
        }))
    }
}

struct SaveLibrary;

#[async_trait::async_trait]
impl AdminTool for SaveLibrary {
    fn name(&self) -> &'static str {
        TOOL_SAVE_CODE_LIBRARY
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Applications)
    }

    fn description(&self, _catalog: &Catalog, grants: &Grants) -> String {
        let permitted = if grants.edit {
            "You may create and change libraries."
        } else {
            "You may **not** create or change a library, so this tool refuses every \
             call — say so rather than retrying."
        };
        format!(
            "Create a code library, or replace the code of the one that already has \
             this name in its scope (an application, or the instance). `{ARG_CODE}` \
             is a function body that assigns to `exports` (or `module.exports`); it \
             runs once per importing run with that run's own `db`, `fs`, `fetch`, \
             `user` … so it can do no more than the code importing it, and may \
             itself call `lib()`. Use one for anything two queries or triggers \
             share, rather than copying it. The code is compile-checked (not run) \
             and a syntax error is refused with its line. An edit reaches every \
             importer on its next call — check `used_by` with \
             `{TOOL_DESCRIBE_CODE_LIBRARIES}` first. {permitted}"
        )
    }

    fn parameters(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                ARG_NAME: {
                    "type": "string",
                    "description": "What `lib(\"…\")` imports it by: letters, digits, \
                        `_`, `$` and `-`, starting with a letter, `_` or `$`."
                },
                ARG_CODE: {
                    "type": "string",
                    "description": "JavaScript statements assigning to `exports`, e.g. \
                        `exports.requireUser = () => { if (!user) throw httpError(401, \"sign in\"); return user; };`"
                },
                ARG_DESCRIPTION: { "type": "string", "description": "What it is for." },
                ARG_APPLICATION: {
                    "type": "string",
                    "description": "The application it belongs to, by subdomain. Omit \
                        for an instance-wide library, which every application and \
                        trigger can import."
                }
            },
            "required": [ARG_NAME, ARG_CODE],
            "additionalProperties": false
        })
    }

    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
        let args = arguments(
            args,
            &[ARG_NAME, ARG_CODE, ARG_DESCRIPTION, ARG_APPLICATION],
        )?;
        require_grant(grants.edit, "create or change a code library", GRANT_EDIT)?;
        let name = required(&args, ARG_NAME)?;
        let code = optional_string(&args, ARG_CODE)?
            .ok_or_else(|| Error::invalid(format!("`{ARG_CODE}` is required")))?;
        let subdomain = optional_string(&args, ARG_APPLICATION)?;
        let application = match &subdomain {
            Some(s) => Some(application_id(ctx, s).await?),
            None => None,
        };
        let existing =
            sc_catalog::load_code_library_by_name(ctx.catalog, &name, application).await?;
        let description = match optional_string(&args, ARG_DESCRIPTION)? {
            Some(d) => d,
            None => existing
                .as_ref()
                .map(|l| l.description.clone())
                .unwrap_or_default(),
        };
        let library = CodeLibrary {
            id: existing.as_ref().map_or_else(Uuid::new_v4, |l| l.id),
            name: name.clone(),
            description,
            code,
            application_id: application,
        };
        crate::save_code_library_checked(ctx.catalog, &library)
            .await
            .map_err(|e| Error::invalid(format!("{e}\n\nNothing was saved.")))?;
        let scope = subdomain
            .as_deref()
            .map_or_else(|| "instance-wide".to_owned(), |s| format!("in `{s}`"));
        Ok(json!({
            "saved": name,
            "created": existing.is_none(),
            "scope": scope,
            "note": "Importers see the new code on their next run: `lib(\"".to_owned()
                + &name + "\")`.",
        }))
    }
}

struct DeleteLibrary;

#[async_trait::async_trait]
impl AdminTool for DeleteLibrary {
    fn name(&self) -> &'static str {
        TOOL_DELETE_CODE_LIBRARY
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Applications)
    }

    fn description(&self, _catalog: &Catalog, grants: &Grants) -> String {
        if grants.drop {
            format!(
                "Delete a code library by name (and `{ARG_APPLICATION}`, for an \
                 application's own). Every query and trigger that imports it fails at \
                 its `lib()` call from then on — check `used_by` with \
                 `{TOOL_DESCRIBE_CODE_LIBRARIES}` and say what will break first."
            )
        } else {
            format!(
                "Delete a code library. You are **not** permitted to, so this tool \
                 refuses every call — say so rather than retrying. Changing its code \
                 with `{TOOL_SAVE_CODE_LIBRARY}` is what you can do instead."
            )
        }
    }

    fn parameters(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                ARG_NAME: { "type": "string", "description": "The library's name." },
                ARG_APPLICATION: {
                    "type": "string",
                    "description": "Its application, by subdomain; omit for an \
                        instance-wide library."
                }
            },
            "required": [ARG_NAME],
            "additionalProperties": false
        })
    }

    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
        let args = arguments(args, &[ARG_NAME, ARG_APPLICATION])?;
        require_grant(grants.drop, "delete a code library", GRANT_DROP)?;
        let name = required(&args, ARG_NAME)?;
        let application = match optional_string(&args, ARG_APPLICATION)? {
            Some(s) => Some(application_id(ctx, &s).await?),
            None => None,
        };
        let Some(library) =
            sc_catalog::load_code_library_by_name(ctx.catalog, &name, application).await?
        else {
            return Err(Error::invalid(format!(
                "there is no code library `{name}` there; `{TOOL_DESCRIBE_CODE_LIBRARIES}` \
                 lists them"
            )));
        };
        sc_catalog::delete_code_library(ctx.catalog, library.id).await?;
        Ok(json!({ "deleted": name }))
    }
}

fn required(args: &Map<String, Json>, key: &str) -> Result<String> {
    optional_string(args, key)?
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::invalid(format!("`{key}` is required")))
}

/// The id of the application with this subdomain, or the refusal naming the
/// ones that exist.
async fn application_id(ctx: &ToolContext<'_>, subdomain: &str) -> Result<Uuid> {
    match crate::load_application_by_subdomain(ctx.catalog, subdomain).await? {
        Some(app) => Ok(app.id.0),
        None => {
            let stored = crate::list_applications(ctx.catalog).await?;
            Err(unknown_application(subdomain, &stored))
        }
    }
}
