//! The admin API's endpoints: saltcorn v1 JavaScript modules.
//!
//! `sc-server`'s `handlers::modules` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // A module, like a file store and an LLM provider, exists only as its stored
    // row plus what is on disk, and these endpoints are that row's lifecycle.
    // Two things are different from the others, and both come from the module
    // being *somebody else's code*:
    //
    // - **What it supplies is read from the package, not from the row** — the
    //   actions, their settings and the module's own settings are all in the
    //   listing because they were read at load, never stored, so `npm install`
    //   cannot leave the admin looking at last week's form.
    // - **Everything that went wrong is reported rather than thrown**: a module
    //   that would not load, an action whose name is already taken, a setting of
    //   a type this version does not know. `issues` is that list, and it is why
    //   `listModules` never fails because one module is broken.

    set.register(
        Endpoint::new("listModules", Method::Get, api().lit("modules"))
            .output(TypeSchema::struct_of([
                StructField::new("modules", TypeSchema::array(module_schema())),
                // Where packages are installed on this server, and whether the
                // toolchain that installs them is there — the two things an
                // admin needs before the first install, and neither of which is
                // a property of any module.
                StructField::new("root", TypeSchema::text()),
                StructField::new("npm", TypeSchema::bool()),
                // Present but useless is its own answer: an npm older than the
                // installer's floor cannot resolve the modules root at all, and
                // fails every install with a semver error about a `file:`
                // specifier. Null when npm can install a module; the version
                // that is there and the version that would work when it cannot,
                // because the sentence the tab shows names both.
                StructField::new(
                    "npm_too_old",
                    TypeSchema::optional(TypeSchema::struct_of([
                        StructField::new("version", TypeSchema::text()),
                        StructField::new("minimum", TypeSchema::text()),
                    ])),
                ),
                StructField::new("node", TypeSchema::bool()),
                // The same two questions for the other language (§8): whether
                // there is an interpreter to build the environment with and
                // whether it has pip, asked before an admin types a
                // distribution name. Neither is the same question as whether
                // *this binary* has Python linked in, which is on the
                // Development tab.
                StructField::new("python", TypeSchema::bool()),
                StructField::new("pip", TypeSchema::bool()),
                StructField::new("python_dir", TypeSchema::optional(TypeSchema::text())),
                // The **bundled** catalog: the modules this server ships with
                // and can install from itself, listed whether or not they are
                // installed. It is here rather than on an endpoint of its own
                // because the tab asks one question — "what can this server
                // run, and what is it running" — and two requests to answer it
                // would be two loading states for one screen.
                StructField::new("bundled", TypeSchema::array(bundled_module_schema())),
            ]))
            // Tagged (§13.6) behind the modules area, which is off unless a
            // token or an agent was explicitly given it: what is installed is
            // somebody else's code.
            .mcp(
                McpTag::new(
                    "List the modules installed on this server — npm packages and \
                 Python distributions — with what each supplies (actions, \
                 functions, table and model providers) and anything wrong with \
                 it; whether npm, python and pip are available to install more; \
                 where the Python environment is; and the bundled modules this \
                 server ships with. A Python package installed here is importable \
                 from every Python code body (`run_python_code` triggers, Python \
                 API queries), so this is also the list of third-party libraries \
                 Python code can use.",
                )
                .in_area(Area::Modules),
            )
            .auth(AuthRequirement::admin()),
    );

    // **Install**: `npm install` in the modules root, then load. The body is the
    // two things an admin types — which kind of source, and the specifier — and
    // everything else about the module is discovered from the package.
    //
    // A **bundled** module is the same endpoint with nothing typed: `source` is
    // `bundled` and `location` is the catalog id the listing above gave, which
    // is what makes the Install button on a catalog card one click. The
    // language comes from the catalog, and so do the permissions the module is
    // granted — the card printed them beside the button.
    set.register(
        Endpoint::new("installModule", Method::Post, api().lit("modules"))
            .input(TypeSchema::struct_of([
                StructField::new("source", TypeSchema::text()),
                StructField::new("location", TypeSchema::text()),
                // `javascript` when it is not sent, which is what every caller
                // written before there was a second language means (§8) — and
                // for a bundled module, the catalog's answer overrides it.
                StructField::new("language", TypeSchema::optional(TypeSchema::text())),
            ]))
            .output(module_schema())
            .mcp(
                McpTag::new(
                    "Install a module, or upgrade one by installing it again. \
                 `language` is `python` or `javascript` (the default). `source` is \
                 `pypi` for a Python distribution from PyPI, `npm` for an npm \
                 package, `local` for a directory on the server's disk, or \
                 `bundled` for one of the modules `listModules` lists under \
                 `bundled` (then `location` is its id). `location` is the \
                 specifier: e.g. `requests`, `pandas>=2.2` or `httpx[http2]` for \
                 PyPI. Any Python distribution may be installed, not only \
                 Saltcorn plugins: a plain library such as `requests` supplies \
                 nothing to the Modules list but becomes importable from every \
                 Python code body. Its dependencies come with it. The package \
                 runs with the server's own privileges, so install only what the \
                 administrator asked for.",
                )
                .in_area(Area::Modules)
                .needs(Grant::Create),
            )
            .auth(AuthRequirement::admin()),
    );

    // **Configure**: the module's own settings, the object v1's `actions(cfg)`
    // is called with, and its **permissions** — what its worker may reach (§2).
    // A save reloads the module, so the next run of any of its actions uses the
    // new value; a permissions save also *moves* it, onto a worker built with
    // the new set, because a permission set belongs to an isolate.
    //
    // Tagged in the modules area: a setting written straight into `_fd_modules`
    // reaches neither the module's actions nor its services, which read it
    // when the module is loaded — this endpoint is the one way that reloads.
    set.register(
        Endpoint::new(
            "updateModule",
            Method::Put,
            api().lit("modules").param("id", ValueType::Uuid),
        )
        .input(TypeSchema::struct_of([
            StructField::new("configuration", TypeSchema::optional(TypeSchema::json())),
            // Present only when the admin edited them, so the settings form and
            // the permissions form are two saves rather than one form that has
            // to carry the other's fields to avoid clearing them.
            StructField::new("permissions", TypeSchema::optional(TypeSchema::json())),
        ]))
        .output(module_schema())
        .mcp(
            McpTag::new(
                "Change an installed module's settings (`configuration`, an object of \
                 the fields its settings form has — `listModules` shows each module's \
                 current values, secrets as `••••••••`) and/or its `permissions` \
                 (`{ net, read, write, env }`, lists of what its worker may reach). \
                 `id` is the module's id from `listModules`. Each part is changed only \
                 when sent, and `configuration` replaces the whole object: send every \
                 setting, with a secret left as `••••••••` to keep it. The module is \
                 reloaded and its services restarted, so its actions and services use \
                 the new values at once.",
            )
            .in_area(Area::Modules)
            .needs(Grant::Edit),
        )
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteModule",
            Method::Delete,
            api().lit("modules").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // **Reload**: re-read every installed package and rebuild the action set.
    // The developer's button — a module installed from a local checkout is a
    // symlink, so editing the checkout changes the module and nothing else has
    // to happen for this to pick it up.
    //
    // Its own path rather than `modules/reload`, which would sit where an id
    // goes.
    set.register(
        Endpoint::new("reloadModules", Method::Post, api().lit("modules-reload"))
            .output(TypeSchema::struct_of([StructField::new(
                "modules",
                TypeSchema::int(),
            )]))
            .auth(AuthRequirement::admin()),
    );
}

/// One installed module, as the Modules tab reads it: the row, what the package
/// turned out to supply, and everything wrong with it.
///
/// `configuration` is **redacted** (§11.1): a module's `password` field is a
/// secret like any other, so what crosses the wire is the sentinel and a save
/// that returns it unchanged keeps what is stored.
fn module_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        // `javascript` or `python`: which host loads it, which package manager
        // installed it, and whether `permissions` applies to it at all (§10).
        StructField::new("language", TypeSchema::text()),
        StructField::new("source", TypeSchema::text()),
        StructField::new("location", TypeSchema::text()),
        StructField::new("version", TypeSchema::optional(TypeSchema::text())),
        StructField::new("configuration", TypeSchema::json()),
        // What its worker may reach (§2): `{ net, read, write, env }`, every
        // one an allow-list and every empty one meaning *nothing*. Closed
        // unless an admin granted something, and reported here so the tab can
        // say what a module can do as well as what it supplies.
        StructField::new("permissions", TypeSchema::json()),
        // The module's own settings, from its `configuration_workflow` — the
        // same declaration a file store's backend sends, so the form is
        // generic.
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
        StructField::new("actions", TypeSchema::array(module_action_schema())),
        // The functions it supplies (§4a): what a code body calls through
        // `modfn` and what a formula hoists, with the signature v1 declared —
        // which is what the code editor's generated types read.
        StructField::new("functions", TypeSchema::array(module_function_schema())),
        // The table providers it supplies (§8.3) — names only, because what a
        // provider *asks for* belongs to the table being created and is on
        // `listTableProviders`.
        StructField::new("table_providers", TypeSchema::array(TypeSchema::text())),
        // The tables it declares (TODO-imap M4), created when it loaded.
        StructField::new("tables", TypeSchema::array(TypeSchema::text())),
        // The application tables it reads because its settings name them
        // (TODO-payments P4.1): read only, by its services and webhooks.
        StructField::new("reads", TypeSchema::array(TypeSchema::text())),
        // The services it runs in the background (TODO-imap M5), each with its
        // state: `running`, `restarting` (after a failure, with `last_error`)
        // or `stopped`.
        StructField::new(
            "services",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("description", TypeSchema::text()),
                StructField::new("state", TypeSchema::text()),
                StructField::new("restarts", TypeSchema::int()),
                StructField::new("last_error", TypeSchema::optional(TypeSchema::text())),
            ])),
        ),
        // The webhooks it receives (TODO-payments P1.2): `path` is where each
        // is served on the admin host, which the admin registers with the
        // service that calls it.
        StructField::new(
            "webhooks",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("description", TypeSchema::text()),
                StructField::new("path", TypeSchema::text()),
            ])),
        ),
        // The v1 routes it serves, with the path each is served at on the
        // admin host (`/_feldspar/routes/<module><url>`).
        StructField::new(
            "routes",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("method", TypeSchema::text()),
                StructField::new("url", TypeSchema::text()),
                StructField::new("path", TypeSchema::text()),
            ])),
        ),
        // The model providers it supplies — names only, for the same reason:
        // what a provider *asks for* belongs to the model being fitted and is
        // on `listModelProviders`.
        StructField::new("model_providers", TypeSchema::array(TypeSchema::text())),
        // The stream providers it supplies (TODO "Streams" §12) — names only,
        // for the same reason again: what one *asks for* belongs to the stream
        // being created and is on `listStreamProviders`.
        StructField::new("stream_providers", TypeSchema::array(TypeSchema::text())),
        // The view patterns it supplies (TODO "Saltcorn UI" 11.1) — names only:
        // what one *asks for* is its configuration wizard, a call per step on
        // `viewConfigStep`.
        StructField::new("view_patterns", TypeSchema::array(TypeSchema::text())),
        // The content-security sources it suggests (TODO-payments P1.4): an
        // application allows one when its admin puts the `key` in the
        // application's `csp_sources` attribute. `sources` is directive → hosts.
        StructField::new(
            "csp_sources",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("key", TypeSchema::text()),
                StructField::new("name", TypeSchema::text()),
                StructField::new("description", TypeSchema::text()),
                StructField::new("sources", TypeSchema::json()),
            ])),
        ),
        // What it also supplies and this version does not load: `{key, count}`,
        // so the tab can say "also supplies 1 table provider (not yet
        // supported)".
        StructField::new(
            "unsupported",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("key", TypeSchema::text()),
                StructField::new("count", TypeSchema::optional(TypeSchema::int())),
            ])),
        ),
        StructField::new("issues", TypeSchema::array(TypeSchema::text())),
        // Whether the package loaded at all. A module with `loaded: false`
        // supplies nothing and its `issues` say why.
        StructField::new("loaded", TypeSchema::bool()),
        StructField::new("api_version", TypeSchema::optional(TypeSchema::int())),
    ])
}

/// One entry in the **bundled catalog**: a module this server ships with.
///
/// Not a `module_schema` with fields left empty. A bundled module that has not
/// been installed has no row, no version, no configuration and supplies nothing
/// — what it has is a card: a heading, a sentence, what it would give, and what
/// installing it would download and grant. `name` is the link between the two,
/// because that is what the installed row is keyed by: a client shows an entry
/// as installed when a module in the same response carries the same name.
fn bundled_module_schema() -> TypeSchema {
    TypeSchema::struct_of([
        // The catalog id — `rss` — which is what `installModule` is given as
        // `location` for a `bundled` source.
        StructField::new("id", TypeSchema::text()),
        // The package's own name once installed — `@feldspar/rss`.
        StructField::new("name", TypeSchema::text()),
        StructField::new("language", TypeSchema::text()),
        StructField::new("title", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // What it supplies, one sentence each, written for the card rather than
        // read from the package: nothing has been installed, so there is no
        // package to read.
        StructField::new("supplies", TypeSchema::array(TypeSchema::text())),
        // What installing it downloads — the dependencies that are deliberately
        // not in the release, so an admin knows the click reaches a registry.
        StructField::new("installs", TypeSchema::array(TypeSchema::text())),
        // The tables installing it creates (TODO-imap M4.6) and uninstalling
        // keeps — each also named in `supplies`.
        StructField::new("tables", TypeSchema::array(TypeSchema::text())),
        // The services it runs in the background (TODO-imap M5): the card says
        // so before the click.
        StructField::new("services", TypeSchema::array(TypeSchema::text())),
        // The webhooks it receives (TODO-payments P1.2), with the path each will
        // be served at once installed.
        StructField::new(
            "webhooks",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("path", TypeSchema::text()),
            ])),
        ),
        // What it will be granted: the same `{ net, read, write, env }` shape a
        // module's own permissions have, so one renderer draws both. Always
        // closed for a Python module — there is nothing to enforce it (§10).
        StructField::new("permissions", TypeSchema::json()),
        // Whether a module with this name is already installed, which is what
        // decides whether the card carries a button or a tick.
        StructField::new("installed", TypeSchema::bool()),
    ])
}

/// One function a module supplies, with the signature v1 declared for it.
///
/// `is_async` is v1's own `isAsync` and decides nothing about how the function
/// is called — everything crosses the host seam awaited. It is here because it
/// is what a signature in the code editor says, and it is v1's word.
fn module_function_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("is_async", TypeSchema::bool()),
        StructField::new(
            "arguments",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                // v1's type name (`String`, `Integer`, `Object`), absent when
                // the module declared none — a guess would read as a promise.
                StructField::new("type", TypeSchema::optional(TypeSchema::text())),
            ])),
        ),
    ])
}

/// One action a module supplies, with the settings it declares.
fn module_action_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
    ])
}
