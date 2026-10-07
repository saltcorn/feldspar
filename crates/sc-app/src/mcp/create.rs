//! Starting an application from nothing (§13.6): `create_file_store` and
//! `create_application`.
//!
//! The rest of this surface edits what is there. These two are what let a
//! person who has nothing yet say "build me a to-do list" to the chat copilot or
//! to an external coding agent and get a working first draft without opening a
//! form: a place for the code, and the application record with its scaffolded
//! project and its builder agent. Once the tables exist, `update_application`
//! connects them ([`super::update`]).
//!
//! ## The same handlers as the admin's buttons
//!
//! Creating a file store and an application are **not** reimplemented here. A
//! create is a sequence — make the store, save the record, scaffold the project
//! with its generated client, create the builder agent, mount what can be
//! mounted — and two of its steps need the agent registry and the mount registry,
//! which only the server holds. So these tools call the server's own
//! `createFileStore`, `runBackendOperation` and `createApplication` handlers
//! through the catalog's [`AdminHost`](sc_catalog::AdminHost). What an agent
//! creates is exactly what the Create button would have created, refused in the
//! same words, and a context with no server (a CLI command) says so rather than
//! creating half an application.
//!
//! ## What comes back is where to go next
//!
//! `create_application` answers with the absolute **project directory** — an
//! external agent carries on building there with its own file tools — and the
//! name of the **builder agent** the framework created, which the chat copilot
//! hands the code to as a sub-agent. The tables come first either way: the
//! generated client is typed against the application's table subset, so code
//! written before the tables exist is code written against nothing.

use std::sync::Arc;

use sc_api::mcp::{
    AdminTool, Area, ToolContext, arguments, optional_bool, optional_string, require_grant,
};
use sc_api::schema_edit::{GRANT_CREATE, Grants};
use sc_catalog::{AdminCall, AdminHost, Catalog, NEW_LOCAL_FILE_STORE};
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};

use super::update::TOOL_UPDATE_APP;
use super::{TOOL_DESCRIBE_APPS, required_str};
use crate::framework::CFG_STORE;
use crate::location::{app_project_dir, store_dir};
use crate::react::{CFG_PROJECT, REACT_FRAMEWORK};

/// Creates a local or git file store for an application's code.
pub const TOOL_CREATE_STORE: &str = "create_file_store";
/// Creates an application: its store, its scaffolded project and its builder.
pub const TOOL_CREATE_APP: &str = "create_application";

/// The two tools, in the order a build uses them.
pub fn create_tools() -> Vec<Arc<dyn AdminTool>> {
    vec![Arc::new(CreateStore), Arc::new(CreateApp)]
}

const ARG_NAME: &str = "name";
const ARG_BACKEND: &str = "backend";
const ARG_URL: &str = "url";
const ARG_BRANCH: &str = "branch";
const ARG_KEY_PATH: &str = "key_path";
const ARG_GENERATE_KEY: &str = "generate_deploy_key";
const ARG_DESCRIPTION: &str = "description";
const ARG_SUBDOMAIN: &str = "subdomain";
const ARG_FRAMEWORK: &str = "framework";
const ARG_FRAMEWORK_CONFIG: &str = "framework_config";
const ARG_STORE: &str = "file_store";
const ARG_PROJECT: &str = "project";
const ARG_TABLES: &str = "tables";

/// The API every application created here gets: REST at `/api`, which is also
/// what the React scaffold's sign-in calls.
const DEFAULT_API_PROVIDER: &str = sc_api::REST_PROVIDER;
const DEFAULT_API_MOUNT: &str = "/api";

// --- create_file_store --------------------------------------------------------

struct CreateStore;

#[async_trait::async_trait]
impl AdminTool for CreateStore {
    fn name(&self) -> &'static str {
        TOOL_CREATE_STORE
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Applications)
    }

    fn description(&self, _catalog: &Catalog, _grants: &Grants) -> String {
        format!(
            "Create a file store: a directory on this server that an application's \
             source code lives in. You rarely need this — `{TOOL_CREATE_APP}` makes \
             a new local store for the application when you name none. Use it when \
             the person asks for the code to live in a **git repository**: \
             `backend: \"git\"` with the repository's `url` clones it, and the \
             application is then created with `file_store` naming this store.\n\n\
             A private repository reached over SSH needs a deploy key the \
             repository accepts. Call once with `{ARG_GENERATE_KEY}: true`: \
             nothing is created, and you get a public key — ask the person to add \
             it to the repository's deploy keys **with write access**, wait until \
             they say it is done, then call again with the `{ARG_KEY_PATH}` you \
             were given. A public `https://` repository needs no key.\n\n\
             Answers with the store's name and its absolute `directory`."
        )
    }

    fn parameters(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                ARG_NAME: {
                    "type": "string",
                    "description":
                        "The store's name: lower-case letters, digits and `-`, \
                         usually the application's subdomain (`todo`).",
                },
                ARG_BACKEND: {
                    "type": "string",
                    "enum": [sc_files::LOCAL_BACKEND, sc_files::GIT_BACKEND],
                    "description":
                        "`local` (the default) is a plain directory on this \
                         server; `git` is a clone of a repository.",
                },
                ARG_URL: {
                    "type": "string",
                    "description": "git only: the repository to clone.",
                },
                ARG_BRANCH: {
                    "type": "string",
                    "description": "git only: the branch; omit for the default branch.",
                },
                ARG_KEY_PATH: {
                    "type": "string",
                    "description":
                        "git only: the SSH private key to clone with, as a \
                         previous call with `generate_deploy_key` returned it.",
                },
                ARG_GENERATE_KEY: {
                    "type": "boolean",
                    "description":
                        "git only: generate a deploy key and return its public \
                         half without creating anything.",
                },
                ARG_DESCRIPTION: {
                    "type": "string",
                    "description": "What the store is for, in one line.",
                },
            },
            "required": [ARG_NAME],
            "additionalProperties": false,
        })
    }

    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
        let args = arguments(
            args,
            &[
                ARG_NAME,
                ARG_BACKEND,
                ARG_URL,
                ARG_BRANCH,
                ARG_KEY_PATH,
                ARG_GENERATE_KEY,
                ARG_DESCRIPTION,
            ],
        )?;
        require_grant(grants.create, "create a file store", GRANT_CREATE)?;
        let host = require_host(ctx, TOOL_CREATE_STORE)?;
        let name = required_str(&args, ARG_NAME)?;
        let backend = optional_string(&args, ARG_BACKEND)?
            .map(|b| b.trim().to_lowercase())
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| sc_files::LOCAL_BACKEND.to_owned());
        let description = optional_string(&args, ARG_DESCRIPTION)?.unwrap_or_default();
        let user = ctx.user.map(|u| u.id);

        let config = match backend.as_str() {
            sc_files::LOCAL_BACKEND => {
                for git_only in [ARG_URL, ARG_BRANCH, ARG_KEY_PATH, ARG_GENERATE_KEY] {
                    if args.contains_key(git_only) {
                        return Err(Error::invalid(format!(
                            "`{git_only}` is a setting of a git store; a local store \
                             takes only a name"
                        )));
                    }
                }
                // Where the admin's "Suggest a directory" would put it: under
                // the server's data directory, named after the store.
                let suggested = host
                    .call_admin(
                        AdminCall::new(
                            "runBackendOperation",
                            json!({ "name": name, "config": {}, "input": {} }),
                        )
                        .param("backend", sc_files::LOCAL_BACKEND)
                        .param("operation", sc_files::OP_SUGGEST_DIR)
                        .user(user),
                    )
                    .await?;
                suggested
                    .get("config")
                    .cloned()
                    .unwrap_or_else(|| json!({}))
            }
            sc_files::GIT_BACKEND => {
                let url = required_str(&args, ARG_URL)?;
                let mut config = Map::new();
                config.insert(sc_files::CFG_URL.to_owned(), json!(url));
                if let Some(branch) = optional_string(&args, ARG_BRANCH)?
                    .map(|b| b.trim().to_owned())
                    .filter(|b| !b.is_empty())
                {
                    config.insert(sc_files::CFG_BRANCH.to_owned(), json!(branch));
                }
                if optional_bool(&args, ARG_GENERATE_KEY)? == Some(true) {
                    let generated = host
                        .call_admin(
                            AdminCall::new(
                                "runBackendOperation",
                                json!({ "name": name, "config": config, "input": {} }),
                            )
                            .param("backend", sc_files::GIT_BACKEND)
                            .param("operation", sc_files::OP_GENERATE_KEY)
                            .user(user),
                        )
                        .await?;
                    let config = generated.get("config").cloned().unwrap_or(Json::Null);
                    return Ok(json!({
                        "created": false,
                        "public_key": config.get(sc_files::CFG_PUBLIC_KEY),
                        ARG_KEY_PATH: config.get(sc_files::CFG_KEY_PATH),
                        "next": format!(
                            "Nothing was created yet. Ask the person to add the \
                             public key above to the deploy keys of {url} with \
                             write access. When they confirm it is added, call \
                             `{TOOL_CREATE_STORE}` again with the same name, url \
                             and branch and `{ARG_KEY_PATH}` set to the value \
                             above."
                        ),
                    }));
                }
                if let Some(key) = optional_string(&args, ARG_KEY_PATH)?
                    .map(|k| k.trim().to_owned())
                    .filter(|k| !k.is_empty())
                {
                    config.insert(sc_files::CFG_KEY_PATH.to_owned(), json!(key));
                }
                Json::Object(config)
            }
            other => {
                return Err(Error::invalid(format!(
                    "`{other}` is not a backend this tool creates; use `local` or `git`"
                )));
            }
        };

        let created = host
            .call_admin(
                AdminCall::new(
                    "createFileStore",
                    json!({
                        "name": name,
                        "backend": backend,
                        "description": description,
                        "config": config,
                    }),
                )
                .user(user),
            )
            .await?;
        let directory = store_dir(ctx.catalog, &name, "")
            .map(|p| json!(p.display().to_string()))
            .unwrap_or(Json::Null);
        Ok(json!({
            "created": name,
            "backend": backend,
            "directory": directory,
            "store": created,
            "next": format!(
                "Create the application with `{TOOL_CREATE_APP}`, passing \
                 `{ARG_STORE}: \"{name}\"`."
            ),
        }))
    }
}

// --- create_application -------------------------------------------------------

struct CreateApp;

#[async_trait::async_trait]
impl AdminTool for CreateApp {
    fn name(&self) -> &'static str {
        TOOL_CREATE_APP
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Applications)
    }

    fn description(&self, _catalog: &Catalog, _grants: &Grants) -> String {
        format!(
            "Create a new application, served on its own subdomain — what to do \
             when the person asks you to build an app, a site or a tool and \
             `{TOOL_DESCRIBE_APPS}` shows none that is it. In one call it makes \
             a file store for the code (a new local one unless `{ARG_STORE}` names \
             an existing store), saves the application with a REST API at `/api`, \
             **scaffolds** the project — a React + TypeScript + Vite app with a \
             generated, typed client for that API in `src/feldspar/` and sign-in \
             already wired — and creates the application's **builder agent**, a \
             coding agent working in that project.\n\n\
             The framework is `react` unless the person asked for something else. \
             When the request is vague about technology (\"build me a to-do \
             list\"), use `react` with a new local store; do not ask.\n\n\
             Answers with the `subdomain`, the absolute `project_dir` the code is \
             in, and the builder agent's name. Then: create the tables with \
             `edit_schema`, connect them with `{TOOL_UPDATE_APP}`'s `tables` (the \
             generated client is typed against the tables connected), and only then write \
             the pages."
        )
    }

    fn parameters(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                ARG_NAME: {
                    "type": "string",
                    "description": "The application's display name (`Todo list`).",
                },
                ARG_SUBDOMAIN: {
                    "type": "string",
                    "description":
                        "Where it is served: lower-case letters, digits and `-`. \
                         Derived from the name when omitted.",
                },
                ARG_DESCRIPTION: {
                    "type": "string",
                    "description": "What it is for, in a sentence.",
                },
                ARG_FRAMEWORK: {
                    "type": "string",
                    "description":
                        "`react` (the default, and the right choice unless the \
                         person named another), or another installed framework.",
                },
                ARG_STORE: {
                    "type": "string",
                    "description":
                        "An existing file store to put the code in — one \
                         `create_file_store` made for a git repository, say. \
                         Omit it to get a new local store named after the \
                         subdomain.",
                },
                ARG_PROJECT: {
                    "type": "string",
                    "description":
                        "The sub-directory of the store for the project; blank \
                         (the default) for the store's root.",
                },
                ARG_FRAMEWORK_CONFIG: {
                    "type": "object",
                    "description":
                        "Settings of a framework other than `react`, by key. Not \
                         needed for `react`.",
                },
                ARG_TABLES: {
                    "type": "array",
                    "items": { "type": "string" },
                    "description":
                        "Existing tables the application's API serves. Usually \
                         omitted: create the tables afterwards and connect them \
                         with `update_application`.",
                },
            },
            "required": [ARG_NAME],
            "additionalProperties": false,
        })
    }

    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
        let args = arguments(
            args,
            &[
                ARG_NAME,
                ARG_SUBDOMAIN,
                ARG_DESCRIPTION,
                ARG_FRAMEWORK,
                ARG_STORE,
                ARG_PROJECT,
                ARG_FRAMEWORK_CONFIG,
                ARG_TABLES,
            ],
        )?;
        require_grant(grants.create, "create an application", GRANT_CREATE)?;
        let host = require_host(ctx, TOOL_CREATE_APP)?;
        let name = required_str(&args, ARG_NAME)?;
        let subdomain = match optional_string(&args, ARG_SUBDOMAIN)?
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
        {
            Some(given) => given,
            None => free_subdomain(ctx.catalog, &subdomain_from(&name)).await?,
        };
        let framework = optional_string(&args, ARG_FRAMEWORK)?
            .map(|f| f.trim().to_owned())
            .filter(|f| !f.is_empty())
            .unwrap_or_else(|| REACT_FRAMEWORK.to_owned());
        let config = framework_config(&framework, &args)?;
        let tables = string_list(&args, ARG_TABLES)?;

        let body = json!({
            "name": name,
            "subdomain": subdomain,
            "description": optional_string(&args, ARG_DESCRIPTION)?.unwrap_or_default(),
            "framework": { "name": framework, "config": config },
            "tables": tables,
            "file_stores": [],
            "apis": [{ "provider": DEFAULT_API_PROVIDER, "mount": DEFAULT_API_MOUNT }],
        });
        let created = host
            .call_admin(AdminCall::new("createApplication", body).user(ctx.user.map(|u| u.id)))
            .await?;

        let app = crate::load_application_by_subdomain(ctx.catalog, &subdomain)
            .await?
            .ok_or_else(|| {
                Error::msg(format!(
                    "the application `{subdomain}` was reported created but is not stored"
                ))
            })?;
        let mut out = json!({
            "created": subdomain,
            "name": app.name,
            "framework": app.framework.name,
            "builder_agent": created.get("agent"),
        });
        match app_project_dir(ctx.catalog, &app) {
            Ok(dir) => out["project_dir"] = json!(dir.display().to_string()),
            Err(e) => out["project_dir_error"] = json!(e.to_string()),
        }
        // The handler's own news, passed on: a scaffold or an agent that could
        // not be made is reported beside the application, never instead of it.
        for key in [
            "created_file_stores",
            "scaffolded",
            "scaffold_error",
            "agent_error",
            "building",
            "mounted",
            "mount_error",
        ] {
            if let Some(value) = created.get(key) {
                out[key] = value.clone();
            }
        }
        out["next"] = json!(format!(
            "1. Create the tables the app needs with `edit_schema`, in one batch. \
             2. Connect them with `{TOOL_UPDATE_APP}` so the API serves them and \
             the generated client in src/feldspar/ is typed for them. \
             3. Write the pages in the project directory{}. \
             4. Build it so it is served at the subdomain.",
            match created.get("agent").and_then(Json::as_str) {
                Some(agent) => format!(" (or hand that to the builder agent `{agent}`)"),
                None => String::new(),
            }
        ));
        Ok(out)
    }
}

/// The framework settings to send: a store and a project for `react`, or what
/// was given for another framework — with every store setting left unset
/// asking for a new local store, as the admin's form does.
fn framework_config(framework: &str, args: &Map<String, Json>) -> Result<Json> {
    let store = optional_string(args, ARG_STORE)?
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let mut config = match args.get(ARG_FRAMEWORK_CONFIG) {
        None | Some(Json::Null) => Map::new(),
        Some(Json::Object(o)) => o.clone(),
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{ARG_FRAMEWORK_CONFIG}` should be an object, got {other}"
            )));
        }
    };
    if framework == REACT_FRAMEWORK {
        let project = optional_string(args, ARG_PROJECT)?.unwrap_or_default();
        config.insert(CFG_PROJECT.to_owned(), json!(project.trim()));
    }
    let spec = crate::framework_config_spec(framework)?;
    for setting in sc_catalog::file_store_settings(&spec) {
        let given = config
            .get(&setting)
            .and_then(Json::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !given {
            let value = match (&store, setting == CFG_STORE) {
                (Some(store), true) => store.clone(),
                _ => NEW_LOCAL_FILE_STORE.to_owned(),
            };
            config.insert(setting, json!(value));
        }
    }
    Ok(Json::Object(config))
}

/// A subdomain made from a display name: lower-case ASCII letters and digits,
/// runs of anything else as one `-`.
pub(crate) fn subdomain_from(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() {
        "app".to_owned()
    } else {
        out
    }
}

/// `wanted`, or `wanted2`, `wanted3`… — the first no application uses.
async fn free_subdomain(catalog: &Catalog, wanted: &str) -> Result<String> {
    let taken: Vec<String> = crate::list_applications(catalog)
        .await?
        .into_iter()
        .map(|a| a.subdomain)
        .collect();
    let mut candidate = wanted.to_owned();
    let mut n = 2;
    while taken.contains(&candidate) {
        candidate = format!("{wanted}{n}");
        n += 1;
    }
    Ok(candidate)
}

// --- shared -------------------------------------------------------------------

/// The server's admin handlers, or the refusal that says this context has none.
fn require_host(ctx: &ToolContext<'_>, tool: &str) -> Result<Arc<dyn AdminHost>> {
    ctx.catalog.admin_host().ok_or_else(|| {
        Error::config(format!(
            "`{tool}` needs the running server, and {} is not connected to one; \
             create it from the admin's Applications screen instead",
            ctx.actor
        ))
    })
}

/// An optional list of names, trimmed, empty ones dropped.
fn string_list(args: &Map<String, Json>, key: &str) -> Result<Vec<String>> {
    match args.get(key) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subdomain_is_made_from_the_display_name() {
        assert_eq!(subdomain_from("Todo list"), "todo-list");
        assert_eq!(subdomain_from("  My   Blog! "), "my-blog");
        assert_eq!(subdomain_from("!!!"), "app");
    }

    #[test]
    fn react_gets_a_new_local_store_unless_one_is_named() {
        let args = Map::new();
        let config = framework_config(REACT_FRAMEWORK, &args).unwrap();
        assert_eq!(config[CFG_STORE], json!(NEW_LOCAL_FILE_STORE));
        assert_eq!(config[CFG_PROJECT], json!(""));

        let mut args = Map::new();
        args.insert(ARG_STORE.to_owned(), json!("todo-repo"));
        let config = framework_config(REACT_FRAMEWORK, &args).unwrap();
        assert_eq!(config[CFG_STORE], json!("todo-repo"));
    }
}
