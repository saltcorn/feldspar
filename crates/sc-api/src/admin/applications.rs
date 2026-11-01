//! The admin API's endpoints: applications: CRUD, building and targets, frameworks, API providers
//! and build hosts.
//!
//! `sc-server`'s `handlers::applications` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // An application is created in the admin UI and exists only as its stored
    // row (§13.2); these endpoints are the row ⇄ mounted-app path the SPA drives.
    // Create/update/delete manage the definition; `build` builds and mounts it
    // live (§13.2 "no restart"); the build's outcome — including a bundler's
    // diagnostics on failure — comes back as an Application error (§16).

    set.register(
        Endpoint::new("listApplications", Method::Get, api().lit("applications"))
            .output(TypeSchema::array(application_schema()))
            .mcp(
                McpTag::new(
                    "List the applications: what each is called, the subdomain it \
                 serves on, its framework and project directory, the API \
                 providers it exposes and the tables and triggers behind them. \
                 The `id` here is what `buildApplication` names.",
                )
                .in_area(Area::Applications),
            )
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("createApplication", Method::Post, api().lit("applications"))
            .input(application_input_schema())
            .output(created_application_schema())
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateApplication",
            Method::Put,
            api().lit("applications").param("id", ValueType::Uuid),
        )
        .input(application_input_schema())
        .output(application_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteApplication",
            Method::Delete,
            api().lit("applications").param("id", ValueType::Uuid),
        )
        // `agent` names the builder agent that was deleted with the application
        // (§13.3), when there was one to delete — an application's builder can do
        // nothing once the application is gone, and the admin should be told it
        // went rather than discover it missing.
        .output(TypeSchema::struct_of([
            StructField::new("deleted", TypeSchema::bool()),
            StructField::new("agent", TypeSchema::optional(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Build (and mount) an application. On success the app is serving on its
    // subdomain; the response reports the build log. A failed build leaves the
    // previously mounted version up and comes back as an Application error whose
    // message carries the bundler's own diagnostics (§16).
    set.register(
        Endpoint::new(
            "buildApplication",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("build"),
        )
        .query([QueryParam::new("quick", ValueType::Bool)])
        .output(build_result_schema())
        // Tagged (§13.6) because of the one gap re-projection leaves: a schema
        // change rewrites an application's generated client but runs no bundler,
        // so an agent that changed a schema needs a way to rebuild what it
        // changed. `edit_schema`'s result names the applications that want one.
        .mcp(
            McpTag::new(
                "Build an application and mount it live: regenerates its typed \
             client from the current schema, runs its framework's build, and \
             serves the result on its subdomain with no restart. Run this after \
             a schema change that `edit_schema` reported as affecting an \
             application with a build. A failed build is a result rather than a \
                 refusal: `built` is false, `log` is what the build tools said \
                 and `diagnostics` lists the file, line and message of each \
                 error to fix. The previously built version keeps serving until \
                 one succeeds.",
            )
            .in_area(Area::Applications)
            // Building writes an application's generated client and replaces
            // what its subdomain serves. That is a change to what is there,
            // which is `allow_edit` — not a create, whatever the method says.
            .needs(Grant::Edit)
            // And a build that did not compile is news about the application,
            // not a refusal of the call: the tool result carries the tools'
            // output with its diagnostics parsed out, which is what an agent
            // that just changed a schema has to read to fix what it broke.
            .is_a_build(),
        )
        .auth(AuthRequirement::admin()),
    );

    // **Deep clean**: delete an application's installed dependencies (a `react`
    // app's `node_modules`) and build it again, which installs them from
    // scratch. For the tree a plain build cannot fix — an interrupted install,
    // a corrupted cache, dependencies changed by hand. The result is a build's,
    // because that is what it ends in; a framework that installs nothing is
    // refused. Not an MCP tool: it is slow, and it is the admin's remedy for a
    // broken machine rather than a step in building an application.
    set.register(
        Endpoint::new(
            "deepCleanApplication",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("deep-clean"),
        )
        .output(build_result_schema())
        .auth(AuthRequirement::admin()),
    );

    // Build one of the targets an application's framework offers beside its web
    // bundle — an Android APK. A native build is minutes long, so this **starts**
    // it and answers at once with the job, `running`; the UI then asks
    // `getApplicationTargetBuild` until it is done. A second start while one is
    // running answers that one rather than starting another in the same project.
    // Not a mount: the result is a file left in the application's store. Not
    // offered over MCP: nothing an agent changing a schema needs to do.
    set.register(
        Endpoint::new(
            "buildApplicationTarget",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("targets")
                .param("target", ValueType::Text)
                .lit("build"),
        )
        .output(target_build_schema())
        .auth(AuthRequirement::admin()),
    );

    // Run an operation a target declares — what a button under the target's
    // settings does, such as generating a signing keystore. The module makes the
    // files and settings; the server writes the files into the application's
    // store (never over an existing one) and saves the settings on it. The body
    // is the form's current framework settings, unsaved edits included, so the
    // module sees what the admin sees.
    set.register(
        Endpoint::new(
            "runApplicationTargetOperation",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("targets")
                .param("target", ValueType::Text)
                .lit("operations")
                .param("operation", ValueType::Text),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "config",
            TypeSchema::json(),
        )]))
        .output(TypeSchema::struct_of([
            StructField::new("message", TypeSchema::text()),
            StructField::new("store", TypeSchema::text()),
            StructField::new("files", TypeSchema::array(TypeSchema::text())),
            // Whether the store is a git repository, whose next commit would
            // carry the files.
            StructField::new("git_repo", TypeSchema::bool()),
            // The settings it set, as now stored, secrets masked.
            StructField::new("settings", TypeSchema::json()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // The latest build of a target, running or finished — what the UI polls, and
    // what it asks after a reload to find a build still running. A 404 when this
    // process has not built that target since it started.
    set.register(
        Endpoint::new(
            "getApplicationTargetBuild",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("targets")
                .param("target", ValueType::Text)
                .lit("build"),
        )
        .output(target_build_schema())
        .auth(AuthRequirement::admin()),
    );

    // Rewrite an application's **generated** code from its current definition —
    // `src/feldspar/**`: the typed client, the hooks, the schema and the README
    // (§13.3). No bundler runs; this is the "if the API definition changes, the
    // client code must be updated automatically" path (decision 10) with a
    // button on it, for the times an admin wants it *now* rather than at the
    // next change.
    //
    // A project directory that is **empty** is scaffolded instead — an app whose
    // store was unreachable when it was created, or whose tree somebody deleted,
    // has nothing to regenerate, and a `src/feldspar/` with no project around it
    // could not build. Which of the two happened is in the response, because
    // writing a whole project is not the same news as rewriting four files.
    set.register(
        Endpoint::new(
            "updateApplicationClient",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("client"),
        )
        .output(TypeSchema::struct_of([
            StructField::new("scaffolded", TypeSchema::bool()),
            StructField::new("files", TypeSchema::array(TypeSchema::text())),
            StructField::new("log", TypeSchema::text()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Run one GraphQL operation against a **mounted** application's own GraphQL
    // provider — what the admin UI's explorer (§13.4) is built on.
    //
    // It is an admin endpoint rather than a browser request to the app's mount
    // because the admin SPA is served under `connect-src 'self'`: a `fetch` from
    // the base domain to `blog.example.com/graphql` is a cross-origin request
    // the page's own policy forbids, and relaxing that policy to let one screen
    // talk to every subdomain is a poor trade for a debugging tool.
    //
    // **The operation runs as the signed-in admin**, through the very same
    // `ApiProvider::handle` a request to the app's mount reaches — same schema,
    // same limits, same authorization at resolve time. The explorer therefore
    // has exactly the authority of the person using it, which is the property
    // that makes it a debugging tool rather than a back door; the screen says so
    // in as many words.
    //
    // The output is `json`: a GraphQL response is `{data, errors, extensions}`
    // whose `data` is the shape the *caller's document* asked for, which no
    // `TypeSchema` can describe ahead of time. That is the same reason the
    // provider's own `graphqlQuery` endpoint declares it.
    set.register(
        Endpoint::new(
            "runApplicationGraphql",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("graphql"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("query", TypeSchema::text()),
            StructField::new("variables", TypeSchema::optional(TypeSchema::json())),
            StructField::new("operationName", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(TypeSchema::json())
        .auth(AuthRequirement::admin()),
    );

    // The registered frameworks with their settings spec, so the create/edit
    // form can render controls for a framework it knows nothing about (§13.3).
    set.register(
        Endpoint::new("listFrameworks", Method::Get, api().lit("frameworks"))
            .output(TypeSchema::array(framework_info_schema()))
            .auth(AuthRequirement::admin()),
    );

    // The registered API providers, so the application form offers them as a
    // **list** rather than a free-text box (§13.4). A provider name is the one
    // field of an application whose typo is not caught until the app is mounted,
    // where it becomes "unknown API provider" on a save that appeared to work.
    // The same move `listFrameworks` makes, for the same reason.
    set.register(
        Endpoint::new("listApiProviders", Method::Get, api().lit("api-providers"))
            .output(TypeSchema::array(api_provider_info_schema()))
            .auth(AuthRequirement::admin()),
    );

    // Prepare one custom SQL query and report the columns the **database** says
    // it returns (§13.4, decision 5), without storing anything.
    //
    // Saving already does this — a query that will not prepare cannot be saved —
    // so why a second route? Because the editor needs the answer *before* the
    // save: the columns are what the admin's generated client method will
    // return, and finding out by saving the whole application means finding out
    // about a typo in one `SELECT` by having every other edit on the screen
    // refused with it. It is the same call on the same catalog; only the moment
    // differs.
    set.register(
        Endpoint::new(
            "describeCustomQuery",
            Method::Post,
            api().lit("custom-queries").lit("describe"),
        )
        .input(custom_query_input_schema())
        .output(TypeSchema::struct_of([StructField::new(
            "columns",
            TypeSchema::array(query_column_schema()),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // The machines that build an application's targets for this server: a Mac
    // for iOS while the server runs on Linux. Its agent authenticates with the
    // token `createBuildHost` answers, once, on `/build-agent/*` and nothing
    // else. Deleting a host is how its token is revoked.

    set.register(
        Endpoint::new("listBuildHosts", Method::Get, api().lit("build-hosts"))
            .output(TypeSchema::array(build_host_schema()))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("createBuildHost", Method::Post, api().lit("build-hosts"))
            .input(build_host_input_schema())
            .output(TypeSchema::struct_of([
                StructField::new("host", build_host_schema()),
                // Shown once: the table keeps only its hash.
                StructField::new("token", TypeSchema::text()),
            ]))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateBuildHost",
            Method::Put,
            api().lit("build-hosts").param("id", ValueType::Uuid),
        )
        .input(build_host_input_schema())
        .output(build_host_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteBuildHost",
            Method::Delete,
            api().lit("build-hosts").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );
}

/// A build host as the admin API answers it: never its token or the token's hash.
fn build_host_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("targets", TypeSchema::array(TypeSchema::text())),
        StructField::new("created_at", TypeSchema::timestamp()),
        StructField::new(
            "last_seen_at",
            TypeSchema::optional(TypeSchema::timestamp()),
        ),
        // Heard from within the last three heartbeats, and not turned away.
        StructField::new("online", TypeSchema::bool()),
        // What its agent last said: version, OS, readiness per target, and
        // `refused` when the server turned it away (an agent too old).
        StructField::new("report", TypeSchema::optional(TypeSchema::json())),
    ])
}

/// What adding or changing a build host takes.
fn build_host_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("targets", TypeSchema::array(TypeSchema::text())),
    ])
}

/// One API provider enabled for an app, on a sub-path.
fn api_config_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("provider", TypeSchema::text()),
        StructField::new("mount", TypeSchema::text()),
        // The provider's own settings, opaque JSON here for the reason a
        // framework's `config` is: what the keys are is the *provider's*
        // declaration (`listApiProviders`' `config_spec`), and the form renders
        // that rather than a shape frozen into this schema.
        StructField::new("config", TypeSchema::json()),
    ])
}

/// A statically-served store subdirectory, on a sub-path.
fn static_dir_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("mount", TypeSchema::text()),
        StructField::new("store", TypeSchema::text()),
        StructField::new("path", TypeSchema::text()),
    ])
}

/// An upload slot (TODO F2, §2.3): where a resumable upload lands, who may
/// start one, how large and of which types, and the code query that decides
/// the rest. `max_bytes` 0 is the server's own limit; `hook` null is none.
fn upload_slot_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("store", TypeSchema::text()),
        StructField::new("folder", TypeSchema::text()),
        StructField::new("min_role", TypeSchema::int()),
        StructField::new("max_bytes", TypeSchema::int()),
        StructField::new("mime_allow", TypeSchema::array(TypeSchema::text())),
        StructField::new("hook", TypeSchema::optional(TypeSchema::text())),
    ])
}

/// An agent an application exposes to its client (§11.7): for chat, single
/// prompts or both, to signed-in callers or — with `anonymous` — to anybody the
/// agent's own floor admits, showing its tools to the extent `tools` says
/// (`none` | `names` | `full`). `prompt_template` null takes free text.
fn agent_exposure_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("chat", TypeSchema::bool()),
        StructField::new("prompt", TypeSchema::bool()),
        StructField::new("anonymous", TypeSchema::bool()),
        StructField::new("tools", TypeSchema::text()),
        StructField::new("prompt_template", TypeSchema::optional(TypeSchema::text())),
    ])
}

/// The fields common to an application on the wire — everything but its id. The
/// nested `csp` and `attributes` are opaque JSON (a directive→sources map and a
/// sparse bag respectively), matching how they are stored (§13.2).
fn application_fields() -> Vec<StructField> {
    vec![
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("subdomain", TypeSchema::text()),
        StructField::new("framework", framework_ref_schema()),
        StructField::new(
            "extra_frameworks",
            TypeSchema::array(framework_ref_schema()),
        ),
        StructField::new("tables", TypeSchema::array(TypeSchema::text())),
        // Of those, the ones whose generic row API the app serves — none unless
        // a table is opted in by name.
        StructField::new("api_tables", TypeSchema::array(TypeSchema::text())),
        StructField::new("file_stores", TypeSchema::array(TypeSchema::text())),
        // The triggers this app exposes as endpoints (§10.2), by name — the same
        // opt-in subset shape the tables and stores have.
        StructField::new("triggers", TypeSchema::array(TypeSchema::text())),
        // …and the streams it exposes for observation (TODO "Streams" §10),
        // the same subset by the same rule: named, or not reachable.
        StructField::new("streams", TypeSchema::array(TypeSchema::text())),
        StructField::new("apis", TypeSchema::array(api_config_schema())),
        StructField::new("static_dirs", TypeSchema::array(static_dir_schema())),
        StructField::new("uploads", TypeSchema::array(upload_slot_schema())),
        StructField::new("agents", TypeSchema::array(agent_exposure_schema())),
        StructField::new("csp", TypeSchema::json()),
        StructField::new("attributes", TypeSchema::json()),
        // Who may use the application at all (TODO F9): the least privileged
        // role let in, the most privileged role held while using it, and a
        // formula over `user` that must hold (empty for none).
        StructField::new("min_role", TypeSchema::int()),
        StructField::new("max_role", TypeSchema::int()),
        StructField::new("access_if", TypeSchema::text()),
    ]
}

/// An application as returned by the API: its id, [`application_fields`], and
/// where its source lives.
///
/// `source` is **derived, not stored**: the server resolves the framework's
/// config to a store and a directory (`app_source_from_config`), so the admin UI
/// can link into the file manager at an app's source without knowing how any
/// framework spells that — `code` states it in five settings and `react` derives
/// it from one. `null` for a framework with no source tree.
fn application_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("id", TypeSchema::uuid())];
    fields.extend(application_fields());
    fields.push(StructField::new(
        "source",
        TypeSchema::optional(app_source_schema()),
    ));
    // Whether the framework has a build step. `false` is a framework that is
    // constructed rather than built (Saltcorn UI): saving the application is
    // its deployment, so the list offers no Build button and shows no
    // "not built yet" state.
    fields.push(StructField::new("builds", TypeSchema::bool()));
    // Whether the build installs the project's dependencies itself (a `react`
    // app's `npm install`), so the list offers Deep clean.
    fields.push(StructField::new("installs", TypeSchema::bool()));
    // Whether the application's source is views and pages (Saltcorn UI), so
    // the screen offers the Views and Pages tabs.
    fields.push(StructField::new("has_views", TypeSchema::bool()));
    // The builds its framework offers beside the web bundle — an Android APK —
    // each a button beside Build. Derived from the framework, like `builds`.
    fields.push(StructField::new(
        "targets",
        TypeSchema::array(TypeSchema::struct_of([
            StructField::new("name", TypeSchema::text()),
            StructField::new("label", TypeSchema::text()),
            // Whether this server can build it now — a state, checked each time
            // the list is asked for — and if not, what it lacks, one sentence
            // each ("`ANDROID_HOME` is not set. …").
            StructField::new(
                "readiness",
                TypeSchema::struct_of([
                    StructField::new("ready", TypeSchema::bool()),
                    StructField::new("missing", TypeSchema::array(TypeSchema::text())),
                    // The required variables no setting fills, which come from
                    // the environment the server was started with, one sentence
                    // each with the value.
                    StructField::new("from_environment", TypeSchema::array(TypeSchema::text())),
                    // The build host it is built on when this server cannot
                    // build it itself (`TODO-build-hosts.md` B2.2).
                    StructField::new("build_host", TypeSchema::optional(TypeSchema::text())),
                ]),
            ),
        ])),
    ));
    TypeSchema::Struct(fields)
}

/// Where an application's source lives: a file store and a directory in it.
fn app_source_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("store", TypeSchema::text()),
        StructField::new("path", TypeSchema::text()),
    ])
}

/// A freshly created application, plus what scaffolding it did (§2.3) and which
/// agent was created to build it (§13.3).
///
/// Only `create` carries these: a `react` app's project is generated on its first
/// save, and the admin should see that it happened — or why it did not — without
/// a second request. `scaffolded` is a summary line; `scaffold_error` explains a
/// scaffold that was refused (an occupied directory, an unreachable store) on an
/// application that was nonetheless created, since the row is valid either way.
/// `agent` and `agent_error` report the builder agent the same way: its name, or
/// why the deployment could not create one (no LLM provider connected). `building`
/// says the server started the application's first build itself, so its subdomain
/// will serve without a restart and without the Build button.
fn created_application_schema() -> TypeSchema {
    let TypeSchema::Struct(mut fields) = application_schema() else {
        unreachable!("application_schema is a struct")
    };
    fields.push(StructField::new(
        "scaffolded",
        TypeSchema::optional(TypeSchema::text()),
    ));
    fields.push(StructField::new(
        "scaffold_error",
        TypeSchema::optional(TypeSchema::text()),
    ));
    // The agent that builds this application, which its framework declares
    // (§13.3): its name when one was created, or why one was not — the same
    // alongside-not-instead-of reporting the scaffold gets, and for the same
    // reason. The application is created either way.
    fields.push(StructField::new(
        "agent",
        TypeSchema::optional(TypeSchema::text()),
    ));
    fields.push(StructField::new(
        "agent_error",
        TypeSchema::optional(TypeSchema::text()),
    ));
    // Whether the server started the application's **first build** as part of
    // creating it (§13.2). Creating an application is what deploys it — the boot
    // path built every stored application, so anything else made a restart part
    // of creating one — but a first build is `npm install` plus a bundler, which
    // is not a thing to hold this response open for. So it runs in the background
    // and this says to expect the subdomain to start serving shortly. Absent for
    // an application with nothing to build.
    fields.push(StructField::new(
        "building",
        TypeSchema::optional(TypeSchema::bool()),
    ));
    // The local file stores created for this application because a store
    // setting asked for a new one (`sc_catalog::NEW_LOCAL_FILE_STORE`) — named
    // after the subdomain, in the directory "Suggest a directory" would have
    // picked. Absent when nothing was created.
    fields.push(StructField::new(
        "created_file_stores",
        TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
    ));
    TypeSchema::Struct(fields)
}

/// The body accepted when creating or updating an application: the same fields
/// minus the id (server-assigned on create, taken from the path on update).
fn application_input_schema() -> TypeSchema {
    TypeSchema::Struct(application_fields())
}

/// One build of a target, as the admin UI polls it: where it has got to, where
/// its log is from the moment it starts, and — once finished — the artifact or
/// the error.
fn target_build_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("target", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        // `queued` (waiting for a build host), `running`, `succeeded` or `failed`.
        StructField::new("status", TypeSchema::text()),
        // The build host it is built on; none for a build on this server.
        StructField::new("build_host", TypeSchema::optional(TypeSchema::text())),
        StructField::new("store", TypeSchema::text()),
        StructField::new("log_path", TypeSchema::text()),
        StructField::new("started_at", TypeSchema::timestamp()),
        StructField::new("finished_at", TypeSchema::optional(TypeSchema::timestamp())),
        StructField::new("artifact", TypeSchema::optional(TypeSchema::text())),
        StructField::new("size", TypeSchema::optional(TypeSchema::int())),
        // The end of the log, once finished; the whole of it is at `log_path`.
        StructField::new("log", TypeSchema::optional(TypeSchema::text())),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
    ])
}

/// The outcome of a build: whether it built, whether its source is a git repo,
/// and the bundler's log. (A *failed* build is not this shape — it is an
/// Application error whose message carries the diagnostics, §16.)
fn build_result_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("built", TypeSchema::bool()),
        StructField::new("git_repo", TypeSchema::bool()),
        StructField::new("log", TypeSchema::text()),
        // The application's checklists, run after the build when its
        // `check_after_build` setting asks for it (TODO-checklists C4.3); null
        // when they were not run.
        StructField::new(
            "checks",
            TypeSchema::optional(TypeSchema::array(TypeSchema::struct_of([
                StructField::new("id", TypeSchema::uuid()),
                StructField::new("checklist", TypeSchema::uuid()),
                StructField::new("checklist_name", TypeSchema::text()),
                StructField::new("status", TypeSchema::text()),
            ]))),
        ),
    ])
}

/// A registered API provider as the application form needs it: the name it is
/// stored under, how to present it, the sub-path it is usually mounted at, and
/// the settings it takes.
///
/// The `config_spec` is the same [`form_field_schema`] a framework's is, and it
/// is here for the same reason (§13.3): the form renders whatever the provider
/// declares, so GraphQL's aggregation switch is a control on a screen that knows
/// nothing about GraphQL, and a provider that grows a setting grows a control
/// without the admin UI being touched.
fn api_provider_info_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // What the form fills the mount box in with when this provider is
        // picked, so the common case is no typing at all.
        StructField::new("default_mount", TypeSchema::text()),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
        // Whether this provider serves custom SQL queries, i.e. whether the
        // application form offers the query editor for it. Declared by the
        // provider for the same reason its settings are: the form renders what
        // it is told rather than checking for a provider by name.
        StructField::new("supports_custom_queries", TypeSchema::bool()),
    ])
}

/// The body `describeCustomQuery` takes: one custom query as the editor holds
/// it, plus the tables the application it belongs to declares. `language` is
/// `sql` when absent; `code` is the source in whichever language it names.
///
/// It is the stored [`CustomQuery`](crate::CustomQuery) shape rather than "just
/// the SQL and the parameters" so that the *whole* refusal an eventual save
/// would give arrives from the check button: a name a table endpoint already
/// holds and a path a table's own routes already answer are both about this
/// query, and learning about them at save time — after the SQL has been declared
/// fine — is two round trips to fix one query. `tables` is the application's
/// declared subset; an application not yet created sends the ones typed into the
/// form, and an empty list simply skips those two rules.
fn custom_query_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("method", TypeSchema::text()),
        StructField::new("path", TypeSchema::text()),
        StructField::new("language", TypeSchema::optional(TypeSchema::text())),
        StructField::new("code", TypeSchema::text()),
        StructField::new(
            "params",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("type", TypeSchema::text()),
                StructField::new("required", TypeSchema::optional(TypeSchema::bool())),
            ])),
        ),
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
        // `json` when absent (TODO F2.9/F2.10).
        StructField::new("body_type", TypeSchema::optional(TypeSchema::text())),
        StructField::new("response_type", TypeSchema::optional(TypeSchema::text())),
        // A code query's declared answer, in the `returns` type language
        // (TODO F4.1), and its rate limit (TODO F4.3).
        StructField::new("returns", TypeSchema::optional(TypeSchema::json())),
        StructField::new(
            "rate_limit",
            TypeSchema::optional(TypeSchema::struct_of([
                StructField::new("per_ip", TypeSchema::optional(TypeSchema::text())),
                StructField::new("per_user", TypeSchema::optional(TypeSchema::text())),
            ])),
        ),
        StructField::new(
            "tables",
            TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
        ),
    ])
}

/// One column of a custom query's result, as the database described it: the name
/// it arrives under in the JSON, and the wire type it was mapped to.
fn query_column_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("type", TypeSchema::text()),
    ])
}
