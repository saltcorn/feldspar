//! The application half of the administrative surface (§13.6): three tools over
//! an application's **custom SQL queries** (§13.4). Creating an application is
//! in [`create`], and changing its connected tables, static directories and CSP
//! in [`update`].
//!
//! The other six tools of that surface live in [`sc_api::mcp`], and this file
//! would too but for the layering: these three read and write an
//! [`Application`], whose storage is this crate's and which `sc-api` — a layer
//! below — cannot name. So they are [`AdminTool`]s like the other six, and
//! [`tool_set`] is where the ten become one set. That is also the reason the
//! set is a list of trait objects rather than a `match`.
//!
//! [`describe_applications`](TOOL_DESCRIBE_APPS) reads what is served and what
//! each API already answers, [`save_api_query`](TOOL_SAVE_QUERY) writes one query
//! and [`delete_api_query`](TOOL_DELETE_QUERY) removes one.
//!
//! ## Why *this* is the application tool, and not "edit the application"
//!
//! An application record carries a subdomain, a framework and its settings, a
//! table subset, file stores, exposed triggers, static directories and a CSP.
//! Some of those are the agent's to write and the rest are not: a framework's
//! `store` and `source` are where somebody's code lives, and a subdomain is a
//! DNS record somebody else configured. The **custom SQL query** is the part of
//! an application that is genuinely a piece of *building* — the escape hatch for
//! the report the row layer cannot express — and it is the part an admin most
//! wants to ask for in a sentence: "give the app an endpoint that returns each
//! author with their book count". So that is what this file offers to write.
//! The connected tables, the static directories and the CSP are changed by
//! `update_application` beside it, because an external agent building an app
//! needs them too; its CSP section is behind `allow_access_changes`, since a
//! CSP is a security boundary.
//!
//! ## The same save path as everything else
//!
//! [`crate::save_application`] is the one authority, exactly as
//! [`sc_api::schema_edit`] is for the schema and [`sc_action::save_trigger`] is
//! for the triggers. It runs the same validation the admin's own form and
//! `feldspar api add-query` run — one statement, declared parameters matching the
//! used ones, a path no table route already answers, a name no client method
//! already has — and then **prepares every query against the database**, which is
//! both the last validation and the typing: a statement Postgres will not prepare
//! comes back carrying Postgres's own message and *nothing is stored*, and one it
//! will is stored with the result columns the database reported.
//!
//! That is why the tool result carries `returns`: the model asked for a report
//! and is told the shape the database says it produces, in the same turn, without
//! running it against anybody's rows.
//!
//! ## Which API, and why the trait does not decide
//!
//! A query belongs to one API of one application, and
//! [`crate::select_api`] is the shared rule for finding it — the same function
//! `feldspar api add-query` calls. An application with two APIs that serve custom
//! queries is **ambiguous**, and ambiguity is refused rather than resolved:
//! picking one would be picking which client method appears where.
//!
//! ## What is *not* live afterwards
//!
//! A mounted application's providers are built from its record when it is
//! mounted, so a query saved here is served by the running app after its next
//! build — exactly as it is when an admin saves one through the admin API. The
//! tool result says so rather than letting the model report an endpoint that is
//! not answering yet. What *is* immediate is the generated TypeScript client,
//! rewritten here for the same reason the admin API rewrites it: the app's source
//! tree should never disagree with the app's definition.

use std::sync::Arc;

use sc_api::mcp::{
    AdminTool, Area, Areas, ToolContext, ToolSet, arguments, optional_bool, optional_role,
    optional_string, require_grant,
};
use sc_api::schema_edit::{GRANT_ACCESS_CHANGES, GRANT_CREATE, GRANT_DROP, GRANT_EDIT, Grants};
use sc_api::{CustomParam, CustomQuery, Method, ValueType};
use sc_auth::{ROLE_ADMIN, ROLE_PUBLIC};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};

use crate::{ApiConfig, Application};

mod create;
mod libraries;
mod streams;
mod translate;
mod update;

pub use create::{TOOL_CREATE_APP, TOOL_CREATE_STORE};
pub use libraries::{
    TOOL_DELETE_CODE_LIBRARY, TOOL_DESCRIBE_CODE_LIBRARIES, TOOL_SAVE_CODE_LIBRARY,
};
pub use streams::TOOL_EDIT_STREAMS;
pub use translate::{TOOL_DESCRIBE_TRANSLATIONS, TOOL_SAVE_TRANSLATIONS};
pub use update::TOOL_UPDATE_APP;

/// The whole administrative tool surface: the schema's two, the triggers' four
/// and the applications' three, in that order.
///
/// The one constructor both callers use — the built-in `admin_copilot` agent and
/// the administration MCP server — so a tool means the same thing, checks the
/// same grants and is refused in the same words whichever of them asked. It
/// lives here rather than in [`sc_api::mcp`] because this is the lowest layer
/// that can name every tool in it.
pub fn tool_set(grants: Grants, areas: Areas) -> ToolSet {
    ToolSet::core(grants, areas).with(app_tools())
}

/// The application tools alone: reading them, creating one (its store, its
/// connected tables), and its custom SQL queries.
pub fn app_tools() -> Vec<Arc<dyn AdminTool>> {
    let mut tools: Vec<Arc<dyn AdminTool>> = vec![Arc::new(DescribeApps)];
    tools.extend(create::create_tools());
    tools.extend(update::update_tools());
    tools.extend(streams::stream_tools());
    tools.extend(translate::translate_tools());
    tools.push(Arc::new(SaveQuery));
    tools.push(Arc::new(DeleteQuery));
    tools.extend(libraries::library_tools());
    tools
}

/// Read the applications, their APIs and the queries already on them.
struct DescribeApps;

#[async_trait::async_trait]
impl AdminTool for DescribeApps {
    fn name(&self) -> &'static str {
        TOOL_DESCRIBE_APPS
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Applications)
    }

    fn description(&self, _catalog: &Catalog, _grants: &Grants) -> String {
        describe_apps_description()
    }

    fn parameters(&self) -> Json {
        describe_apps_parameters()
    }

    async fn call(&self, ctx: &ToolContext<'_>, _grants: &Grants, args: &Json) -> Result<Json> {
        describe_apps(ctx, args).await
    }
}

/// Write one custom SQL query, or edit the one already holding the name.
struct SaveQuery;

#[async_trait::async_trait]
impl AdminTool for SaveQuery {
    fn name(&self) -> &'static str {
        TOOL_SAVE_QUERY
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Applications)
    }

    fn description(&self, _catalog: &Catalog, grants: &Grants) -> String {
        save_description(grants)
    }

    fn parameters(&self) -> Json {
        save_parameters()
    }

    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
        save(ctx, grants, args).await
    }
}

/// Delete one.
struct DeleteQuery;

#[async_trait::async_trait]
impl AdminTool for DeleteQuery {
    fn name(&self) -> &'static str {
        TOOL_DELETE_QUERY
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Applications)
    }

    fn description(&self, _catalog: &Catalog, grants: &Grants) -> String {
        delete_description(grants)
    }

    fn parameters(&self) -> Json {
        delete_parameters()
    }

    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
        delete(ctx, grants, args).await
    }
}

/// Reads the applications, their APIs and the queries on them.
pub const TOOL_DESCRIBE_APPS: &str = "describe_applications";
/// Creates a custom SQL query, or edits the one already holding the name.
pub const TOOL_SAVE_QUERY: &str = "save_api_query";
/// Deletes one.
pub const TOOL_DELETE_QUERY: &str = "delete_api_query";

/// Which application, by subdomain — the routing key, and the name an admin uses.
const ARG_APPLICATION: &str = "application";
/// Which of its APIs, by mount. Optional when only one serves custom queries.
const ARG_API: &str = "api";
const ARG_NAME: &str = "name";
const ARG_DESCRIPTION: &str = "description";
const ARG_METHOD: &str = "method";
const ARG_PATH: &str = "path";
const ARG_CODE: &str = "code";
const ARG_LANGUAGE: &str = "language";
const ARG_PARAMS: &str = "params";
const ARG_MIN_ROLE: &str = "min_role";
/// `true` is `min_role: 100` said the way people say it (TODO F1.6). Stored
/// as the role; `min_role` stays the one fact.
const ARG_PUBLIC: &str = "public";
/// `json` (default) or `bytes`: what a JavaScript query's `body` is (TODO F2.9).
const ARG_BODY_TYPE: &str = "body_type";
/// `json` (default) or `bytes`: what a code query answers with, which types
/// its client method (TODO F2.9/F2.10).
const ARG_RESPONSE_TYPE: &str = "response_type";
/// A code query's declared JSON response type (TODO F4.1).
const ARG_RETURNS: &str = "returns";
/// `{ per_ip, per_user }` rates (TODO F4.3).
const ARG_RATE_LIMIT: &str = "rate_limit";

/// The arguments [`save`] accepts.
const SAVE_ARGS: [&str; 15] = [
    ARG_RETURNS,
    ARG_RATE_LIMIT,
    ARG_PUBLIC,
    ARG_BODY_TYPE,
    ARG_RESPONSE_TYPE,
    ARG_APPLICATION,
    ARG_API,
    ARG_NAME,
    ARG_DESCRIPTION,
    ARG_METHOD,
    ARG_PATH,
    ARG_CODE,
    ARG_LANGUAGE,
    ARG_PARAMS,
    ARG_MIN_ROLE,
];

/// How this trait's user says which API they mean — what
/// [`select_api`]'s refusal names, because a CLI flag would send a model
/// to a command line it is not standing at.
const HOW_TO_NAME_API: &str = "the `api` argument";

// --- describe_applications ----------------------------------------------------

fn describe_apps_description() -> String {
    format!(
        "List the applications this server serves: what each one is, which \
         tables and triggers it exposes, which APIs it mounts, and — the part \
         you can change — the **custom SQL queries** on each API, with their SQL, \
         their parameters, who may call them and the columns the database says \
         they return. Each also carries its `project_dir` — the absolute \
         directory its source is in on this machine, `null` when that is not on \
         this disk — its `builder_agent`, the coding agent created to build \
         it, the file stores it is connected to, the store folders it serves \
         (`static_dirs`), its upload slots (`uploads`, each with the `route` a \
         client uploads to) and its Content-Security-Policy (`csp`).\n\n\
         Call this before `{TOOL_SAVE_QUERY}`: it tells you the application's \
         subdomain, which API a query goes on, and whether the name you have in \
         mind is already taken. For the tables a query's SQL can read, call \
         `describe_schema` — the SQL runs against the whole database, not only \
         the subset the application exposes through its table routes."
    )
}

fn describe_apps_parameters() -> Json {
    json!({
        "type": "object",
        "properties": {
            ARG_APPLICATION: {
                "type": "string",
                "description":
                    "Describe only the application served on this subdomain. Omit \
                     it to list them all, which is what you want before planning \
                     a change.",
            },
        },
        "additionalProperties": false,
    })
}

async fn describe_apps(ctx: &ToolContext<'_>, args: &Json) -> Result<Json> {
    let args = arguments(args, &[ARG_APPLICATION])?;
    let only = args
        .get(ARG_APPLICATION)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let stored = crate::list_applications(ctx.catalog).await?;
    if let Some(subdomain) = only
        && !stored.iter().any(|a| a.subdomain == subdomain)
    {
        return Err(unknown_application(subdomain, &stored));
    }
    let described: Result<Vec<Json>> = stored
        .iter()
        .filter(|a| only.is_none_or(|subdomain| a.subdomain == subdomain))
        .map(|app| {
            let mut out = application_json(app)?;
            // Where its code is, so an external agent can go on building
            // there, and which agent builds it, so the copilot can delegate.
            out["project_dir"] = match crate::app_project_dir(ctx.catalog, app) {
                Ok(dir) => json!(dir.display().to_string()),
                Err(_) => Json::Null,
            };
            out["builder_agent"] = json!(crate::builder_agent_name(app));
            Ok(out)
        })
        .collect();
    Ok(json!({ "applications": described? }))
}

/// One stored application as the model reads it.
fn application_json(app: &Application) -> Result<Json> {
    let apis: Result<Vec<Json>> = app.apis.iter().map(api_json).collect();
    Ok(json!({
        "id": app.id.0,
        "subdomain": app.subdomain,
        "name": app.name,
        "description": app.description,
        "framework": app.framework.name,
        "tables": app.tables.iter().map(|t| t.0.clone()).collect::<Vec<_>>(),
        "triggers": app.triggers.iter().map(|t| t.0.clone()).collect::<Vec<_>>(),
        "streams": app.streams.iter().map(|s| s.0.clone()).collect::<Vec<_>>(),
        "file_stores": app.file_stores.iter().map(|s| s.0.clone()).collect::<Vec<_>>(),
        "static_dirs": update::static_dirs_json(&app.static_dirs),
        "uploads": app.uploads.iter().map(|s| {
            let mut slot = s.to_json();
            slot["route"] = json!(crate::upload_path(app, &s.name));
            slot
        }).collect::<Vec<_>>(),
        "csp": update::csp_json(&app.csp),
        "apis": apis?,
    }))
}

fn api_json(api: &ApiConfig) -> Result<Json> {
    let serves = crate::serves_custom_queries(&api.provider);
    let queries: Vec<Json> = match serves {
        true => sc_api::custom_queries(&api.config)?
            .iter()
            .map(query_json)
            .collect(),
        // A provider that does not serve them has none to report, and reading
        // its settings for a `queries` key would invent a list nobody stored.
        false => Vec::new(),
    };
    Ok(json!({
        "mount": api.mount,
        "provider": api.provider,
        // Said rather than left to be inferred from the provider's name: which
        // providers take custom queries is a declaration (§13.4), and a model
        // that guessed `graphql` would be refused a turn later.
        "serves_custom_queries": serves,
        "queries": queries,
    }))
}

/// One custom query, with the columns the database reported for it.
fn query_json(query: &CustomQuery) -> Json {
    json!({
        "name": query.name,
        "description": query.description,
        "method": query.method.as_str(),
        "path": query.path,
        "language": query.language.as_str(),
        "code": query.code,
        "params": query.params.iter().map(param_json).collect::<Vec<_>>(),
        "min_role": query.min_role,
        // Said, not left to be known: 100 is the public role, and "who can
        // call this?" is the question a reader of this list is asking.
        "public": query.min_role == ROLE_PUBLIC,
        "body_type": query.body_type.as_str(),
        "response_type": query.response_type.as_str(),
        // Server-written, never declared: the shape the database said this
        // statement produces when it was last saved. Empty means it has not been
        // described, which types the response as opaque rather than lying.
        "columns": query.columns
            .iter()
            .map(|c| json!({ "name": c.name, "type": c.ty.name() }))
            .collect::<Vec<_>>(),
        // A code query's declared response type, as written (TODO F4.1).
        "returns": query.returns.clone().unwrap_or(Json::Null),
        // The stated limit, or null for the default; and what applies.
        "rate_limit": query.rate_limit.as_ref().map(|r| json!({
            "per_ip": r.per_ip,
            "per_user": r.per_user,
        })),
        "effective_rate_limit": effective_rate_limit_json(query),
    })
}

/// The limits a call to `query` is counted against, as rates.
fn effective_rate_limit_json(query: &CustomQuery) -> Json {
    let e = query.effective_rate_limit();
    json!({
        "per_ip": e.per_ip.map(|r| r.to_string()),
        "per_user": e.per_user.map(|r| r.to_string()),
    })
}

fn param_json(param: &CustomParam) -> Json {
    json!({
        "name": param.name,
        "type": param.ty.name(),
        "required": param.required,
    })
}

// --- save_api_query -----------------------------------------------------------

fn save_description(grants: &Grants) -> String {
    let permitted = match (grants.create, grants.edit) {
        (true, true) => "You may add queries and change existing ones.".to_owned(),
        (true, false) => "You may add queries, but not change one that already exists.".to_owned(),
        (false, true) => "You may change existing queries, but not add one.".to_owned(),
        (false, false) => "You may neither add nor change a query; this tool will refuse \
                           every call. Say so rather than retrying."
            .to_owned(),
    };
    let access = match grants.access_changes {
        true => format!(
            "You may set `{ARG_MIN_ROLE}` (or `{ARG_PUBLIC}: true`, the same as \
             `{ARG_MIN_ROLE}: 100`), which decides who may call the endpoint. That changes what users of this deployment can reach, so \
             say plainly what you are about to do before you do it."
        ),
        false => format!(
            "You may **not** set `{ARG_MIN_ROLE}` or `{ARG_PUBLIC}`; a call naming \
             either is refused. \
             Without it a query is admin-only, which is the safe default."
        ),
    };
    format!(
        "Add a custom SQL query to an application's API, or change the one that \
         already has this name — the name is the identity within that API, so \
         saving under a name that exists **edits that query**. It becomes an \
         endpoint of the app at the API's mount plus `{ARG_PATH}`, and a typed \
         method on the app's generated client.\n\n\
         Adding one needs `{ARG_APPLICATION}`, `{ARG_NAME}`, `{ARG_PATH}` and \
         `{ARG_CODE}` (the SQL, unless `{ARG_LANGUAGE}` says otherwise). Editing one needs only `{ARG_APPLICATION}`, `{ARG_NAME}` \
         and what is changing: **anything you omit is left as it is**.\n\n\
         The rules, all of them checked before anything is stored:\n\
         - **One statement.** A custom query is one statement; a migration is not \
           an API endpoint. Use `edit_schema` to change the schema.\n\
         - **Parameters are `:name` in the SQL and declared in `{ARG_PARAMS}`**, \
           and the two must be the same set. They are bound as values, never \
           pasted into the text, so never build a filter by concatenating one in. \
           An optional parameter binds SQL NULL when it is omitted, which makes \
           `WHERE (:q IS NULL OR name = :q)` the idiom for an optional filter.\n\
         - **The path may not be one the app's own tables answer**, and the name \
           may not be one of their client methods.\n\
         - **The statement is prepared against the database.** A query that will \
           not prepare comes back with the database's own message and nothing is \
           saved; one that will is stored with the columns the database says it \
           returns, which come back in `columns`.\n\
         - **A code query declares its answer in `{ARG_RETURNS}`**, so the client \
           method is typed rather than `unknown`.\n\
         - **A public code query is limited to {default_rate} per IP** unless \
           `{ARG_RATE_LIMIT}` says otherwise.\n\n\
         {permitted} {access}\n\n\
         The saved query is served by the running application after its next \
         build — say that rather than reporting a live endpoint.",
        default_rate = sc_api::DEFAULT_PUBLIC_RATE,
    )
}

fn save_parameters() -> Json {
    json!({
        "type": "object",
        "properties": {
            ARG_APPLICATION: {
                "type": "string",
                "description":
                    "The application, by subdomain, as `describe_applications` \
                     lists it.",
            },
            ARG_API: {
                "type": "string",
                "description":
                    "Which of the application's APIs holds the query, by its \
                     mount (`/api`). Omit it when only one of them serves custom \
                     SQL queries, which is the usual case.",
            },
            ARG_NAME: {
                "type": "string",
                "description":
                    "The query's name, unique within the API: letters, digits and \
                     underscores, starting with a letter. It is the method name \
                     on the app's generated client, so name it as one \
                     (`topAuthors`, `salesByMonth`).",
            },
            ARG_DESCRIPTION: {
                "type": "string",
                "description":
                    "What it is for, in one line. It becomes the doc comment on \
                     the generated client's method.",
            },
            ARG_METHOD: {
                "type": "string",
                "description":
                    "The HTTP method. `GET` — the default — runs in a read-only \
                     transaction, so a statement that writes must use another \
                     method. `GET` and `DELETE` take their arguments in the query \
                     string; the rest take a JSON body.",
                "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"],
            },
            ARG_PATH: {
                "type": "string",
                "description":
                    "The sub-path within the API's mount, e.g. \
                     `/reports/top-authors`. Letters, digits, `-`, `_` and `.` in \
                     each segment. It may not start with a segment the app's own \
                     table routes answer.",
            },
            ARG_CODE: {
                "type": "string",
                "description":
                    "The source. For SQL: one statement, with `:name` for each parameter. It \
                     runs against the whole database — `describe_schema` is what \
                     tells you the tables and columns — and inside the caller's \
                     context, so a table with row-level security still filters it.",
            },
            ARG_LANGUAGE: {
                "type": "string",
                "description":
                    "What `code` is written in. `sql` — the default — is one \
                     statement, as described here. `javascript` and `python` make \
                     it a code body instead, run like a trigger's `run_js_code` / \
                     `run_python_code` body, with the request's JSON body as \
                     `body`, its query string as `query` and the caller as `user`; \
                     what it returns is the response. A code body is not prepared, \
                     and a declared parameter need not appear in it; the app's \
                     client method sends whatever it is given either way, but \
                     declare the parameters the body reads so they are typed and \
                     checked. `throw httpError(status, message)` and `return \
                     response({ status, headers, body })` choose the status; any \
                     other throw is a 500 with no stack for non-admin callers. \
                     Its `db`, `fetch`, `fs` and `crypto` are this server's own \
                     API: call `describe_code_api` before writing a `javascript` \
                     body rather than guessing the methods. Prefer a table with an \
                     ownership formula, or SQL, to a code body where either can \
                     express it.",
                "enum": ["sql", "javascript", "python"],
            },
            ARG_BODY_TYPE: {
                "type": "string",
                "description":
                    "`javascript` with POST/PUT/PATCH only. `json` — the default — \
                     makes `body` the parsed JSON object. `bytes` makes `body` a \
                     `Uint8Array` of the raw request body (an image, a PDF, a CSV) \
                     up to the server's --max-request-body; the declared params then \
                     come from the query string, and `request.contentType` says \
                     what was sent. The client method takes a `Blob`, \
                     `ArrayBuffer` or `Uint8Array`. For files larger than a request \
                     body, use an application upload slot instead.",
                "enum": ["json", "bytes"],
            },
            ARG_RESPONSE_TYPE: {
                "type": "string",
                "description":
                    "`javascript` only. `json` — the default — for a body that \
                     returns JSON. `bytes` for one that returns `response({ body: \
                     uint8array, headers: { \"content-type\": … } })` or \
                     `response({ file: fs(store).open(path), filename, download })`: \
                     the client method then resolves to a `Blob`, and a GET query \
                     also gets `<name>Url(params)` for `<img src>`, `<video>` and \
                     download links. A file is streamed with Range support and no \
                     size limit — this is how to write a custom download rule \
                     (password-protected or expiring shares). `stream` for one \
                     that ends in `return live.stream(name, key, { write, watch })`: \
                     it is opened over the app's live socket, not called over HTTP \
                     (a fetch is a 400), and attaches the caller — anonymous too, \
                     if the query is public — to that row's document stream or \
                     internal row stream, with edit/publish rights when `write`. \
                     The query is the whole access check (the stream's min_role and \
                     ownership rule are not applied); it runs again when a table \
                     it read (or named in `watch`) is written, and every minute, \
                     and a run that no longer returns the same handle revokes the \
                     page. This is how to write share links for collaborative \
                     documents. The client method is `name(params, handlers)`; \
                     with `live-doc.ts`, `useQueryDocument(api.name, params)`.",
                "enum": ["json", "bytes", "stream"],
            },
            ARG_RETURNS: {
                "description":
                    "`javascript`/`python` with a JSON answer only: what the body \
                     returns, so the generated client's method resolves to a typed \
                     `<Name>Response` instead of `unknown`. A type is a name — \
                     \"string\", \"int\", \"float\", \"bool\", \"json\" (anything), \
                     \"uuid\", \"date\", \"timestamp\" — or one of \
                     {\"object\": {\"prop\": T, …}}, {\"array\": T}, \
                     {\"optional\": T} (T or null; an optional property may be \
                     left out) and {\"union\": [T, T, …]}. Example: \
                     {\"object\": {\"id\": \"uuid\", \"tags\": {\"array\": \"string\"}}}. \
                     Not enforced on live calls; a run's preview logs a warning when \
                     a response does not match. Send null to remove it.",
            },
            ARG_RATE_LIMIT: {
                "type": ["object", "null"],
                "description":
                    "How often one caller may call it: rates such as \"20/min\", \
                     \"5/15m\" or \"1000/day\", counted per client IP and per \
                     signed-in user. Over either, the call is a 429 with \
                     Retry-After. Omitted (or null), a public code query gets 60/min \
                     per IP and anything else none; `{}` states no limit at all. For \
                     a limit per something else — per share, per email — use \
                     `ratelimit.check(key, …)` inside the body.",
                "properties": {
                    "per_ip": { "type": "string" },
                    "per_user": { "type": "string" },
                },
                "additionalProperties": false,
            },
            ARG_PARAMS: {
                "type": "array",
                "description":
                    "The parameters the SQL uses, in the order a caller reads \
                     them. Every `:name` in the SQL must be declared here and \
                     every declared one must be used. Sending this replaces the \
                     stored list whole rather than merging into it.",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "The `:name` in the SQL, without the colon.",
                        },
                        "type": {
                            "type": "string",
                            "description":
                                "What the caller's value is coerced to before it \
                                 is bound.",
                            "enum": ValueType::ALL.map(|t| t.name()),
                        },
                        "required": {
                            "type": "boolean",
                            "description":
                                "Whether the caller must supply it. An omitted \
                                 optional parameter binds SQL NULL. Defaults to \
                                 true.",
                        },
                    },
                    "required": ["name", "type"],
                    "additionalProperties": false,
                },
            },
            ARG_MIN_ROLE: {
                "type": "integer",
                "description":
                    "Least-privileged role that may call this endpoint, 1 (admin) \
                     to 100 (anyone — **public**, no sign-in). A new query is \
                     admin-only unless this says otherwise.",
                "minimum": 1,
                "maximum": 100,
            },
            ARG_PUBLIC: {
                "type": "boolean",
                "description":
                    "`true` makes the endpoint public — callable by anyone, \
                     signed in or not — and is the same as `min_role: 100`. Send \
                     one or the other; if you send both they must agree. Needs \
                     the same grant as `min_role`.",
            },
        },
        "required": [ARG_APPLICATION, ARG_NAME],
        "additionalProperties": false,
    })
}

async fn save(ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
    let args = arguments(args, &SAVE_ARGS)?;
    let name = required_str(&args, ARG_NAME)?;
    let mut app = load_app(ctx, &args).await?;
    let subdomain = app.subdomain.clone();
    let api = crate::select_api(&mut app, api_mount(&args)?.as_deref(), HOW_TO_NAME_API)?;
    let mount = api.mount.clone();
    let mut queries = sc_api::custom_queries(&api.config)?;

    // Decided before anything is written, and what picks the grant: adding an
    // endpoint and rewriting one an admin wrote are two different permissions
    // over one tool, so an agent allowed to build new queries cannot quietly
    // change an existing one by reusing its name.
    let existing = queries.iter().position(|q| q.name == name);
    match existing {
        Some(_) => require_grant(
            grants.edit,
            "change an existing custom SQL query",
            GRANT_EDIT,
        )?,
        None => require_grant(grants.create, "add a custom SQL query", GRANT_CREATE)?,
    }
    if args.contains_key(ARG_MIN_ROLE) || args.contains_key(ARG_PUBLIC) {
        require_grant(
            grants.access_changes,
            "set who may call a custom SQL query",
            GRANT_ACCESS_CHANGES,
        )?;
    }

    let query = build(&name, existing.map(|i| &queries[i]), &args)?;
    match existing {
        Some(i) => queries[i] = query,
        None => queries.push(query),
    }
    sc_api::set_custom_queries(&mut api.config, &queries)?;

    // The save is the validation *and* the typing: every query the app declares
    // is prepared against the database, and nothing is written unless all of them
    // do. So the application that comes back is the one that was stored, with the
    // columns the database reported — never the one that was sent.
    let saved = crate::save_application(ctx.catalog, &app)
        .await
        .map_err(|e| {
            Error::invalid(format!(
                "{e}\n\nNothing was saved: the application is exactly as it was."
            ))
        })?;

    let mut notes = vec![format!(
        "`{subdomain}` serves this from its next build; the stored definition and \
         the generated client are updated now."
    )];
    // The generated client, rewritten for the same reason the admin API rewrites
    // it on every save: the app's source tree must not disagree with the app's
    // definition. Never fatal — the query is stored and valid, and an app with no
    // source tree (or an unreachable store) is a thing to report, not a reason to
    // say the save failed.
    if let Err(e) = crate::emit_app_client(ctx.catalog, &saved, ctx.triggers).await {
        notes.push(format!(
            "the query is saved, but the application's generated client could not \
             be rewritten: {e}"
        ));
    }

    let stored = stored_query(&saved, &mount, &name)?;
    notes.extend(sc_api::undeclared_params_note(&stored));
    Ok(json!({
        "saved": name,
        "application": subdomain,
        "api": mount,
        "created": existing.is_none(),
        // The whole query as it now stands, not an echo of what was sent: an edit
        // merged into what was stored, and `returns` is the database's answer
        // rather than anybody's declaration.
        "query": query_json(&stored),
        "notes": notes,
    }))
}

/// The query to save: the stored one with the given changes applied, or a new
/// one.
///
/// Built through [`CustomQuery::new`] either way, so the path is normalised on an
/// edit exactly as it is on a create — a rule that lived in the constructor would
/// otherwise apply only to half the writes.
///
/// The columns are deliberately left empty: they are the **database's** to write,
/// and [`save_application`] re-describes every query on every save. A
/// carried-over column list would be the one thing in the record older than the
/// SQL beside it.
fn build(
    name: &str,
    existing: Option<&CustomQuery>,
    args: &Map<String, Json>,
) -> Result<CustomQuery> {
    let method = match optional_string(args, ARG_METHOD)? {
        Some(raw) => parse_method(&raw)?,
        None => existing.map(|q| q.method).unwrap_or(Method::Get),
    };
    let missing = |what: &str| {
        Error::invalid(format!(
            "there is no custom SQL query named `{name}` on this API, so this adds \
             one — which needs `{ARG_PATH}` and `{ARG_CODE}` as well as the name \
             (`{what}` is missing)"
        ))
    };
    let path = match optional_string(args, ARG_PATH)?.filter(|p| !p.trim().is_empty()) {
        Some(path) => path,
        None => existing
            .map(|q| q.path.clone())
            .ok_or_else(|| missing(ARG_PATH))?,
    };
    let code = match optional_string(args, ARG_CODE)?.filter(|s| !s.trim().is_empty()) {
        Some(code) => code,
        None => existing
            .map(|q| q.code.clone())
            .ok_or_else(|| missing(ARG_CODE))?,
    };

    let mut query = CustomQuery::new(name, method, path, code);
    query.language = match optional_string(args, ARG_LANGUAGE)? {
        Some(raw) => {
            serde_json::from_value(Json::String(raw.trim().to_lowercase())).map_err(|_| {
                Error::invalid(format!(
                    "`{ARG_LANGUAGE}` should be `sql`, `javascript` or `python`, got `{raw}`"
                ))
            })?
        }
        None => existing.map(|q| q.language).unwrap_or_default(),
    };
    query.body_type = match optional_string(args, ARG_BODY_TYPE)? {
        Some(raw) => {
            serde_json::from_value(Json::String(raw.trim().to_lowercase())).map_err(|_| {
                Error::invalid(format!(
                    "`{ARG_BODY_TYPE}` should be `json` or `bytes`, got `{raw}`"
                ))
            })?
        }
        None => existing.map(|q| q.body_type).unwrap_or_default(),
    };
    query.response_type = match optional_string(args, ARG_RESPONSE_TYPE)? {
        Some(raw) => {
            serde_json::from_value(Json::String(raw.trim().to_lowercase())).map_err(|_| {
                Error::invalid(format!(
                    "`{ARG_RESPONSE_TYPE}` should be `json`, `bytes` or `stream`, got `{raw}`"
                ))
            })?
        }
        None => existing.map(|q| q.response_type).unwrap_or_default(),
    };
    query.returns = match args.get(ARG_RETURNS) {
        Some(Json::Null) => None,
        // A model may send the type as a JSON string of itself.
        Some(Json::String(text)) if text.trim_start().starts_with('{') => Some(
            serde_json::from_str(text)
                .map_err(|e| Error::invalid(format!("`{ARG_RETURNS}` is not valid JSON: {e}")))?,
        ),
        Some(declared) => Some(declared.clone()),
        None => existing.and_then(|q| q.returns.clone()),
    };
    query.rate_limit = match args.get(ARG_RATE_LIMIT) {
        Some(Json::Null) => None,
        Some(limit) => Some(serde_json::from_value(limit.clone()).map_err(|e| {
            Error::invalid(format!(
                "`{ARG_RATE_LIMIT}` should be an object with `per_ip` and/or \
                 `per_user` rates such as \"20/min\": {e}"
            ))
        })?),
        None => existing.and_then(|q| q.rate_limit.clone()),
    };
    query.description = match args.contains_key(ARG_DESCRIPTION) {
        true => optional_string(args, ARG_DESCRIPTION)?.unwrap_or_default(),
        false => existing.map(|q| q.description.clone()).unwrap_or_default(),
    };
    query.params = match args.contains_key(ARG_PARAMS) {
        true => parse_params(args)?,
        false => existing.map(|q| q.params.clone()).unwrap_or_default(),
    };
    // Admin unless stated, on a create as well as on an edit that does not name
    // it: an access nobody has thought about must not be the one that turns out
    // to be public (§13.4).
    query.min_role = match requested_role(args)? {
        Some(role) => role,
        None => existing.map(|q| q.min_role).unwrap_or(ROLE_ADMIN),
    };
    Ok(query)
}

/// The role floor the arguments ask for: `min_role`, or `public: true` for
/// 100. Both together must agree, and `public: false` says only "not 100" —
/// so on its own it is refused rather than guessed into some other role.
fn requested_role(args: &Map<String, Json>) -> Result<Option<u8>> {
    let role = optional_role(args, ARG_MIN_ROLE)?;
    let public = optional_bool(args, ARG_PUBLIC)?;
    match (role, public) {
        (role, None) => Ok(role),
        (None, Some(true)) => Ok(Some(ROLE_PUBLIC)),
        (Some(role), Some(public)) if (role == ROLE_PUBLIC) == public => Ok(Some(role)),
        (Some(role), Some(public)) => Err(Error::invalid(format!(
            "`{ARG_PUBLIC}: {public}` and `{ARG_MIN_ROLE}: {role}` disagree: public is \
             {ARG_MIN_ROLE} {ROLE_PUBLIC}. Send one of them"
        ))),
        (None, Some(false)) => Err(Error::invalid(format!(
            "`{ARG_PUBLIC}: false` does not say who may call it; send `{ARG_MIN_ROLE}` \
             with the least-privileged role that may (1 is admin)"
        ))),
    }
}

/// The declared parameters, as sent. `null` is an empty list — how a query that
/// no longer takes any says so.
fn parse_params(args: &Map<String, Json>) -> Result<Vec<CustomParam>> {
    let items = match args.get(ARG_PARAMS) {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(items)) => items,
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{ARG_PARAMS}` should be a list of parameters, got {other}"
            )));
        }
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            return Err(Error::invalid(format!(
                "`{ARG_PARAMS}[{i}]` should be an object with `name` and `type`, got {item}"
            )));
        };
        for key in obj.keys() {
            if !["name", "type", "required"].contains(&key.as_str()) {
                return Err(Error::invalid(format!(
                    "`{ARG_PARAMS}[{i}]` has an unknown key `{key}`; a parameter is \
                     `name`, `type` and optionally `required`"
                )));
            }
        }
        let name = optional_string(obj, "name")?
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .ok_or_else(|| Error::invalid(format!("`{ARG_PARAMS}[{i}]` needs a `name`")))?;
        let raw = optional_string(obj, "type")?.ok_or_else(|| {
            Error::invalid(format!(
                "the parameter `{name}` needs a `type`; the types are {}",
                type_names()
            ))
        })?;
        let ty = ValueType::from_name(raw.trim()).ok_or_else(|| {
            Error::invalid(format!(
                "`{raw}` is not a parameter type for `{name}`; the types are {}",
                type_names()
            ))
        })?;
        let param = CustomParam::new(name, ty);
        out.push(match optional_bool(obj, "required")? {
            Some(false) => param.optional(),
            _ => param,
        });
    }
    Ok(out)
}

// --- delete_api_query ---------------------------------------------------------

fn delete_description(grants: &Grants) -> String {
    match grants.drop {
        false => format!(
            "Delete a custom SQL query from an application's API. You are **not** \
             permitted to, so this tool refuses every call — say so rather than \
             retrying. Narrowing what a query returns, or raising its \
             `{ARG_MIN_ROLE}`, with `{TOOL_SAVE_QUERY}` is what you can do instead."
        ),
        true => format!(
            "Delete a custom SQL query by name from an application's API. Its \
             endpoint and its method on the generated client go with it, so any \
             app code calling that method stops compiling — check with \
             `{TOOL_DESCRIBE_APPS}` and say what will break before you do it. \
             Nothing the query ever returned is undone; it only stops being \
             callable."
        ),
    }
}

fn delete_parameters() -> Json {
    json!({
        "type": "object",
        "properties": {
            ARG_APPLICATION: {
                "type": "string",
                "description": "The application, by subdomain.",
            },
            ARG_API: {
                "type": "string",
                "description":
                    "Which of its APIs, by mount. Omit it when only one serves \
                     custom SQL queries.",
            },
            ARG_NAME: {
                "type": "string",
                "description": "The query to delete.",
            },
        },
        "required": [ARG_APPLICATION, ARG_NAME],
        "additionalProperties": false,
    })
}

async fn delete(ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
    let args = arguments(args, &[ARG_APPLICATION, ARG_API, ARG_NAME])?;
    let name = required_str(&args, ARG_NAME)?;
    require_grant(grants.drop, "delete a custom SQL query", GRANT_DROP)?;

    let mut app = load_app(ctx, &args).await?;
    let subdomain = app.subdomain.clone();
    let api = crate::select_api(&mut app, api_mount(&args)?.as_deref(), HOW_TO_NAME_API)?;
    let mount = api.mount.clone();
    let mut queries = sc_api::custom_queries(&api.config)?;
    let Some(i) = queries.iter().position(|q| q.name == name) else {
        return Err(unknown_query(&name, &subdomain, &mount, &queries));
    };
    let was = queries.remove(i);
    sc_api::set_custom_queries(&mut api.config, &queries)?;

    let saved = crate::save_application(ctx.catalog, &app).await?;
    let mut notes = vec![format!(
        "`{subdomain}` stops answering it after its next build; the stored \
         definition and the generated client are updated now."
    )];
    if let Err(e) = crate::emit_app_client(ctx.catalog, &saved, ctx.triggers).await {
        notes.push(format!(
            "the query is deleted, but the application's generated client could \
             not be rewritten: {e}"
        ));
    }
    Ok(json!({
        "deleted": was.name,
        "application": subdomain,
        "api": mount,
        // What it was, because this is the last moment anything can say so and an
        // agent that has just deleted the wrong endpoint should be able to put it
        // back from its own transcript.
        "was": query_json(&was),
        "notes": notes,
    }))
}

// --- shared -------------------------------------------------------------------

/// The application the arguments name, or the refusal that lists the ones there
/// are.
async fn load_app(ctx: &ToolContext<'_>, args: &Map<String, Json>) -> Result<Application> {
    let subdomain = required_str(args, ARG_APPLICATION)?;
    match crate::load_application_by_subdomain(ctx.catalog, &subdomain).await? {
        Some(app) => Ok(app),
        None => {
            let stored = crate::list_applications(ctx.catalog).await?;
            Err(unknown_application(&subdomain, &stored))
        }
    }
}

/// The `api` argument, trimmed, or `None` for "there is only one".
fn api_mount(args: &Map<String, Json>) -> Result<Option<String>> {
    Ok(optional_string(args, ARG_API)?
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty()))
}

fn required_str(args: &Map<String, Json>, key: &str) -> Result<String> {
    optional_string(args, key)?
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::invalid(format!("`{key}` is required")))
}

/// The query as it was **stored**, read back out of the saved application.
///
/// Read back rather than echoed, because the save is what wrote the columns: the
/// difference between what was sent and what is there is exactly the answer the
/// model was asking for.
fn stored_query(app: &Application, mount: &str, name: &str) -> Result<CustomQuery> {
    let api = app
        .apis
        .iter()
        .find(|a| a.mount == mount)
        .ok_or_else(|| Error::msg(format!("the saved application has no API at `{mount}`")))?;
    sc_api::custom_queries(&api.config)?
        .into_iter()
        .find(|q| q.name == name)
        .ok_or_else(|| Error::msg(format!("the saved application has no query named `{name}`")))
}

fn parse_method(raw: &str) -> Result<Method> {
    match raw.trim().to_ascii_uppercase().as_str() {
        "GET" => Ok(Method::Get),
        "POST" => Ok(Method::Post),
        "PUT" => Ok(Method::Put),
        "PATCH" => Ok(Method::Patch),
        "DELETE" => Ok(Method::Delete),
        other => Err(Error::invalid(format!(
            "`{other}` is not an HTTP method; use GET, POST, PUT, PATCH or DELETE"
        ))),
    }
}

fn type_names() -> String {
    ValueType::ALL
        .iter()
        .map(|t| t.name())
        .collect::<Vec<_>>()
        .join(", ")
}

/// "No application on that subdomain", with the ones there are.
fn unknown_application(subdomain: &str, stored: &[Application]) -> Error {
    let names: Vec<&str> = stored.iter().map(|a| a.subdomain.as_str()).collect();
    Error::not_found(format!(
        "no application is served at `{subdomain}`; {}",
        match names.is_empty() {
            true => "there are no applications yet".to_owned(),
            false => format!("the applications are {}", names.join(", ")),
        }
    ))
}

/// "No query called that on this API", with the ones there are.
fn unknown_query(name: &str, subdomain: &str, mount: &str, queries: &[CustomQuery]) -> Error {
    let names: Vec<&str> = queries.iter().map(|q| q.name.as_str()).collect();
    Error::not_found(format!(
        "the API at `{mount}` of `{subdomain}` has no custom SQL query named \
         `{name}`; {}",
        match names.is_empty() {
            true => "it has none".to_owned(),
            false => format!("it has {}", names.join(", ")),
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, Json)]) -> Map<String, Json> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn the_parameter_type_enum_in_the_schema_is_the_live_set() {
        let params = save_parameters();
        let listed = params["properties"][ARG_PARAMS]["items"]["properties"]["type"]["enum"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let expected: Vec<Json> = ValueType::ALL.iter().map(|t| json!(t.name())).collect();
        assert_eq!(listed, expected);
        // An application and a name; everything else is compulsory only when
        // there is nothing stored to leave alone.
        assert_eq!(params["required"], json!([ARG_APPLICATION, ARG_NAME]));
    }

    #[test]
    fn a_new_query_needs_its_sql_and_is_admin_only_unless_asked_otherwise() {
        let err = build(
            "topAuthors",
            None,
            &args(&[(ARG_PATH, json!("/reports/top-authors"))]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(ARG_CODE), "{err}");

        let query = build(
            "topAuthors",
            None,
            &args(&[
                (ARG_PATH, json!("reports/top-authors")),
                (ARG_CODE, json!("select author from books")),
            ]),
        )
        .unwrap();
        assert_eq!(query.min_role, ROLE_ADMIN);
        assert_eq!(query.method, Method::Get);
        // The path is normalised on the way in, on an edit as well as a create,
        // because both go through the same constructor.
        assert_eq!(query.path, "/reports/top-authors");
        assert!(query.columns.is_empty());
    }

    /// TODO F2.9/F2.10: the body and response types are arguments, kept on
    /// an edit that does not name them, refused when misspelt, and reported.
    #[test]
    fn returns_and_rate_limit_are_set_kept_cleared_and_reported() {
        let returns = json!({ "object": { "id": "uuid", "tags": { "array": "string" } } });
        let query = build(
            "tagged",
            None,
            &args(&[
                (ARG_PATH, json!("/tagged")),
                (ARG_LANGUAGE, json!("javascript")),
                (
                    ARG_CODE,
                    json!("return { id: crypto.randomUUID(), tags: [] };"),
                ),
                (ARG_PUBLIC, json!(true)),
                (ARG_RETURNS, returns.clone()),
                (ARG_RATE_LIMIT, json!({ "per_ip": "20/min" })),
            ]),
        )
        .unwrap();
        assert_eq!(query.returns, Some(returns.clone()));
        assert_eq!(
            query.rate_limit.as_ref().and_then(|r| r.per_ip.as_deref()),
            Some("20/min")
        );
        let edited = build("tagged", Some(&query), &args(&[])).unwrap();
        assert_eq!(edited.returns, Some(returns.clone()));
        let reported = query_json(&edited);
        assert_eq!(reported["returns"], returns);
        assert_eq!(reported["rate_limit"]["per_ip"], json!("20/min"));
        assert_eq!(reported["effective_rate_limit"]["per_ip"], json!("20/1m"));

        // A type sent as a string of JSON is the type.
        let as_text = build(
            "tagged",
            Some(&query),
            &args(&[(ARG_RETURNS, json!(returns.to_string()))]),
        )
        .unwrap();
        assert_eq!(as_text.returns, Some(returns));

        // Null clears both: no declared type, and the public default limit.
        let cleared = build(
            "tagged",
            Some(&query),
            &args(&[(ARG_RETURNS, Json::Null), (ARG_RATE_LIMIT, Json::Null)]),
        )
        .unwrap();
        assert_eq!(cleared.returns, None);
        assert_eq!(cleared.rate_limit, None);
        let reported = query_json(&cleared);
        assert_eq!(reported["returns"], Json::Null);
        assert_eq!(reported["effective_rate_limit"]["per_ip"], json!("60/1m"));

        let err = build(
            "tagged",
            Some(&query),
            &args(&[(ARG_RATE_LIMIT, json!({ "perIp": "20/min" }))]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(ARG_RATE_LIMIT), "{err}");
    }

    #[test]
    fn body_and_response_types_are_set_kept_and_reported() {
        let query = build(
            "upload",
            None,
            &args(&[
                (ARG_PATH, json!("/upload")),
                (ARG_METHOD, json!("POST")),
                (ARG_LANGUAGE, json!("javascript")),
                (ARG_CODE, json!("return body.length;")),
                (ARG_BODY_TYPE, json!("bytes")),
                (ARG_RESPONSE_TYPE, json!("Bytes")),
            ]),
        )
        .unwrap();
        assert_eq!(query.body_type, sc_api::BodyType::Bytes);
        assert_eq!(query.response_type, sc_api::ResponseType::Bytes);
        let edited = build("upload", Some(&query), &args(&[])).unwrap();
        assert_eq!(edited.body_type, sc_api::BodyType::Bytes);
        assert_eq!(edited.response_type, sc_api::ResponseType::Bytes);
        let reported = query_json(&edited);
        assert_eq!(reported["body_type"], json!("bytes"));
        assert_eq!(reported["response_type"], json!("bytes"));

        // TODO F7: a stream query.
        let opens = build(
            "openNote",
            None,
            &args(&[
                (ARG_PATH, json!("/open-note")),
                (ARG_METHOD, json!("POST")),
                (ARG_LANGUAGE, json!("javascript")),
                (ARG_CODE, json!("return live.stream(\"note_body\", 1);")),
                (ARG_RESPONSE_TYPE, json!("stream")),
            ]),
        )
        .unwrap();
        assert_eq!(opens.response_type, sc_api::ResponseType::Stream);
        assert_eq!(query_json(&opens)["response_type"], json!("stream"));

        let err = build(
            "upload",
            Some(&query),
            &args(&[(ARG_BODY_TYPE, json!("blob"))]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(ARG_BODY_TYPE), "{err}");
        // What the type means is checked where every query is validated: a
        // bytes body on a GET, or from SQL, is refused by name.
        let get = build("upload", Some(&query), &args(&[(ARG_METHOD, json!("GET"))])).unwrap();
        let err = sc_api::validate_custom_queries(&[get], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("POST, PUT or PATCH"), "{err}");
        let sql = build(
            "upload",
            Some(&query),
            &args(&[(ARG_LANGUAGE, json!("sql")), (ARG_CODE, json!("select 1"))]),
        )
        .unwrap();
        let err = sc_api::validate_custom_queries(&[sql], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("JavaScript"), "{err}");
    }

    #[test]
    fn an_edit_leaves_everything_it_does_not_name() {
        let stored = CustomQuery::new(
            "topAuthors",
            Method::Get,
            "/reports/top-authors",
            "select author from books where year > :since",
        )
        .params([CustomParam::new("since", ValueType::Int)])
        .description("Who wrote most")
        .min_role(40);

        let edited = build(
            "topAuthors",
            Some(&stored),
            &args(&[(ARG_CODE, json!("select author, count(*) from books"))]),
        )
        .unwrap();
        assert_eq!(edited.code, "select author, count(*) from books");
        assert_eq!(edited.path, stored.path);
        assert_eq!(edited.method, stored.method);
        assert_eq!(edited.description, stored.description);
        assert_eq!(edited.params, stored.params);
        assert_eq!(edited.min_role, 40);

        // …and `params: []` is how a query that no longer takes any says so,
        // which is a different thing from omitting it.
        let cleared = build(
            "topAuthors",
            Some(&stored),
            &args(&[(ARG_PARAMS, json!([]))]),
        )
        .unwrap();
        assert!(cleared.params.is_empty());
    }

    #[test]
    fn a_parameter_type_that_is_not_one_is_refused_naming_the_choices() {
        let err = parse_params(&args(&[(
            ARG_PARAMS,
            json!([{ "name": "since", "type": "integer" }]),
        )]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("integer"), "{err}");
        assert!(err.contains("int,"), "{err}");

        let params = parse_params(&args(&[(
            ARG_PARAMS,
            json!([
                { "name": "since", "type": "int" },
                { "name": "q", "type": "text", "required": false },
            ]),
        )]))
        .unwrap();
        assert_eq!(
            params,
            vec![
                CustomParam::new("since", ValueType::Int),
                CustomParam::new("q", ValueType::Text).optional(),
            ]
        );
    }

    #[test]
    fn the_descriptions_say_what_the_grants_do_not_allow() {
        let text = save_description(&Grants::none());
        assert!(text.contains("neither add nor change"), "{text}");
        assert!(text.contains("may **not** set"), "{text}");
        let text = save_description(&Grants::all());
        assert!(text.contains("add queries and change"), "{text}");

        let text = delete_description(&Grants::none());
        assert!(text.contains("**not** permitted"), "{text}");
        // The thing it *can* do instead, so a refusal is not a dead end.
        assert!(text.contains(TOOL_SAVE_QUERY), "{text}");
    }

    #[test]
    fn an_unknown_application_and_an_unknown_query_name_what_exists() {
        let err = unknown_application("blog", &[]).to_string();
        assert!(err.contains("no applications yet"), "{err}");

        let err = unknown_query("topAuthors", "blog", "/api", &[]).to_string();
        assert!(err.contains("it has none"), "{err}");
        let queries = vec![CustomQuery::new("topAuthor", Method::Get, "/r", "select 1")];
        let err = unknown_query("topAuthors", "blog", "/api", &queries).to_string();
        assert!(err.contains("it has topAuthor"), "{err}");
    }
}
