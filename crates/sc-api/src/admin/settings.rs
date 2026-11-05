//! The admin API's endpoints: settings, external identity providers, the admin address, the test
//! email, system information and help.
//!
//! `sc-server`'s `handlers::settings` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(set: &mut EndpointSet) {
    // A help topic (`sc_docs::help`), filled in with what the screen is about and
    // rendered to HTML — v1's question-mark popups. `table` and `event` say what
    // the screen is about: the table a formula or a trigger is on, the event a
    // code body runs in. Both are optional; a topic with neither still renders,
    // only less specifically. An unknown topic is a 404, an unknown table is
    // ignored rather than refused, since a half-filled form can name one that
    // does not exist yet.
    set.register(
        Endpoint::new(
            ids::GET_HELP,
            Method::Get,
            api().lit("help").param("topic", ValueType::Text),
        )
        .query([
            QueryParam::new("table", ValueType::Text),
            QueryParam::new("event", ValueType::Text),
        ])
        .output(TypeSchema::struct_of([
            StructField::new("title", TypeSchema::text()),
            StructField::new("html", TypeSchema::text()),
        ])),
    );

    // The `_fd_config` values an admin edits (§9, §13.5). Two endpoints, and
    // both carry the **declarations** alongside the values, for the same reason
    // the file-store and LLM-provider screens are handed a `config_spec`: the
    // settings screen renders whatever the server declares and knows nothing
    // about any particular setting. Adding one is a Rust declaration and a
    // redeployed server, with no matching change in the SPA.
    //
    // The save returns the settings as they now stand rather than an
    // acknowledgement, so the screen shows what was stored — including the
    // defaults a cleared box fell back to, and the sentinel standing in for a
    // secret it must not be handed back.
    set.register(
        Endpoint::new(ids::GET_SETTINGS, Method::Get, api().lit("settings"))
            .output(settings_schema()),
    );

    set.register(
        Endpoint::new(ids::UPDATE_SETTINGS, Method::Post, api().lit("settings"))
            .input(TypeSchema::struct_of([StructField::new(
                "values",
                TypeSchema::json(),
            )]))
            .output(settings_schema()),
    );

    // OAuth 2.0 providers people sign in with, on the admin UI and in
    // applications. Each one's settings are declared (`getOAuthProviderSpec`),
    // so the tab's form is generic; its client secret is redacted on the way
    // out and the sentinel merged back on save, as an LLM provider's key is.
    // Addressed by name: the name is the slug in the provider's sign-in URL.

    set.register(
        Endpoint::new(
            ids::LIST_O_AUTH_PROVIDERS,
            Method::Get,
            api().lit("oauth-providers"),
        )
        .output(TypeSchema::array(oauth_provider_schema())),
    );

    set.register(
        Endpoint::new(
            ids::GET_O_AUTH_PROVIDER_SPEC,
            Method::Get,
            api().lit("oauth-provider-spec"),
        )
        .output(TypeSchema::struct_of([
            StructField::new("fields", TypeSchema::array(form_field_schema())),
            // The path a provider must be told to send the browser back to, on
            // the admin UI's own host (the screen adds its origin).
            StructField::new("callback_path", TypeSchema::text()),
        ])),
    );

    set.register(
        Endpoint::new(
            ids::CREATE_O_AUTH_PROVIDER,
            Method::Post,
            api().lit("oauth-providers"),
        )
        .input(oauth_provider_input_schema())
        .output(oauth_provider_schema()),
    );

    set.register(
        Endpoint::new(
            ids::UPDATE_O_AUTH_PROVIDER,
            Method::Put,
            api().lit("oauth-providers").param("name", ValueType::Text),
        )
        .input(oauth_provider_input_schema())
        .output(oauth_provider_schema()),
    );

    set.register(
        Endpoint::new(
            ids::DELETE_O_AUTH_PROVIDER,
            Method::Delete,
            api().lit("oauth-providers").param("name", ValueType::Text),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )])),
    );

    // Where the admin UI is served, and whether a browser can follow it there
    // yet (Settings → Development → Admin subdomain). The dialog a move opens
    // polls this until `ready`, and the screens that link to applications read
    // `base_domain` from it, since the admin UI's own host stops being the base
    // domain once it moves.
    //
    // `certificate` is `null` without a base domain; otherwise `state` is
    // `plain_http`, `ready`, `ordering` or `not_covered`, and `message` is what
    // the CA last said (or why the pasted certificate does not do).
    set.register(
        Endpoint::new(
            ids::GET_ADMIN_ADDRESS,
            Method::Get,
            api().lit("settings").lit("admin-address"),
        )
        .output(TypeSchema::struct_of([
            StructField::new("base_domain", TypeSchema::optional(TypeSchema::text())),
            StructField::new("admin_subdomain", TypeSchema::optional(TypeSchema::text())),
            StructField::new("admin_host", TypeSchema::optional(TypeSchema::text())),
            StructField::new("ready", TypeSchema::bool()),
            StructField::new(
                "certificate",
                TypeSchema::optional(TypeSchema::struct_of([
                    StructField::new("state", TypeSchema::text()),
                    StructField::new("message", TypeSchema::optional(TypeSchema::text())),
                ])),
            ),
        ])),
    );

    // A single-use token that signs the admin in on the admin UI's current host
    // — the session cookie is host-only, so following a move would otherwise
    // mean logging in again. `path` is opened on `host`, with the scheme and
    // port the browser already uses.
    set.register(
        Endpoint::new(
            ids::CREATE_ADMIN_HANDOFF,
            Method::Post,
            api().lit("settings").lit("admin-address").lit("handoff"),
        )
        .output(TypeSchema::struct_of([
            StructField::new("host", TypeSchema::text()),
            StructField::new("path", TypeSchema::text()),
        ])),
    );

    // Send one message through the **stored** email settings (§18.2).
    //
    // A section may have an *act* as well as fields, and this is the first one:
    // "are these settings right" is a question only the network can answer, and
    // an admin who has to wait for a trigger to fire to find out has no way to
    // tell a wrong password from a wrong template.
    //
    // It tests what is **saved**, not what is typed — the transport is built
    // from `_fd_config` — so the answer is about the configuration this
    // installation will actually send with. `to` defaults to the signed-in
    // admin's own address, because the admin pressing the button is the one
    // person guaranteed to be able to check whether it arrived.
    set.register(
        Endpoint::new(
            ids::SEND_TEST_EMAIL,
            Method::Post,
            api().lit("settings").lit("email").lit("test"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "to",
            TypeSchema::optional(TypeSchema::text()),
        )]))
        .output(TypeSchema::struct_of([StructField::new(
            "sent_to",
            TypeSchema::text(),
        )])),
    );

    // **Python, as this process has it** (design §15; TODO "The Python code
    // adapter" §7). Read-only, on the Development tab, and none of it is a
    // setting: "why does my Python trigger not work" has three answers — not
    // built with Python, built but told not to start one, running — and every
    // number beside them is a fact about the running process rather than
    // something an admin types. `explanation` is the server's own sentence for
    // whichever state it is in, because which one is true depends on how this
    // binary was built and how it was started.
    set.register(
        Endpoint::new(ids::GET_PYTHON_STATUS, Method::Get, api().lit("python")).output(
            TypeSchema::struct_of([
                StructField::new("state", TypeSchema::text()),
                StructField::new("version", TypeSchema::optional(TypeSchema::text())),
                StructField::new("explanation", TypeSchema::text()),
                // Where the environment is, where a package lands inside it, and
                // which external interpreter builds it (§9). All optional: an
                // interpreter that has not started cannot say which directory
                // its packages would come from, because that depends on its own
                // version.
                StructField::new("dir", TypeSchema::optional(TypeSchema::text())),
                StructField::new("site_packages", TypeSchema::optional(TypeSchema::text())),
                StructField::new("bin", TypeSchema::optional(TypeSchema::text())),
                StructField::new(
                    "packages",
                    TypeSchema::array(TypeSchema::struct_of([
                        StructField::new("name", TypeSchema::text()),
                        StructField::new("version", TypeSchema::optional(TypeSchema::text())),
                    ])),
                ),
                // The admission bound and what is against it, then the leak:
                // a thread that never came back cannot be reclaimed, so the
                // count is here to be seen long before it reaches its limit.
                StructField::new("max_inflight", TypeSchema::int()),
                StructField::new("resident", TypeSchema::int()),
                StructField::new("threads", TypeSchema::int()),
                StructField::new("stuck", TypeSchema::int()),
                StructField::new("max_stuck", TypeSchema::int()),
                // Why the environment is not in use, where that is the case
                // (§9): a virtual environment built by a different Python holds
                // packages this interpreter cannot import — a C extension would
                // crash the server rather than fail to import — so it is left
                // off `sys.path` and this says so, naming both versions.
                StructField::new("env_error", TypeSchema::optional(TypeSchema::text())),
            ]),
        ),
    );

    // **Settings → System**: which feldspar this is and what it is doing to
    // the machine. The build fields are null for a binary not built by
    // `scripts/build-static.sh`; a reading this platform cannot take is null.
    // Read-only, except for the restart beside it.
    set.register(
        Endpoint::new(ids::GET_SYSTEM_INFO, Method::Get, api().lit("system")).output(
            TypeSchema::struct_of([
                StructField::new("version", TypeSchema::text()),
                // `feldspar --version`'s line, for a bug report.
                StructField::new("version_line", TypeSchema::text()),
                StructField::new("build_time", TypeSchema::optional(TypeSchema::text())),
                StructField::new("commit", TypeSchema::optional(TypeSchema::text())),
                StructField::new("platform", TypeSchema::text()),
                StructField::new("pid", TypeSchema::int()),
                StructField::new("started_at", TypeSchema::timestamp()),
                StructField::new("uptime_seconds", TypeSchema::int()),
                StructField::new(
                    "database",
                    TypeSchema::struct_of([
                        StructField::new("product", TypeSchema::optional(TypeSchema::text())),
                        StructField::new("version", TypeSchema::optional(TypeSchema::text())),
                        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
                    ]),
                ),
                StructField::new(
                    "memory",
                    TypeSchema::optional(TypeSchema::struct_of([
                        StructField::new("process_bytes", TypeSchema::optional(TypeSchema::int())),
                        StructField::new("total_bytes", TypeSchema::optional(TypeSchema::int())),
                        StructField::new(
                            "available_bytes",
                            TypeSchema::optional(TypeSchema::int()),
                        ),
                    ])),
                ),
                // Percentages of the whole machine (every core), over a
                // quarter-second sample taken when this is asked.
                StructField::new(
                    "cpu",
                    TypeSchema::optional(TypeSchema::struct_of([
                        StructField::new("process_percent", TypeSchema::Value(ValueType::Float)),
                        StructField::new("system_percent", TypeSchema::Value(ValueType::Float)),
                        StructField::new("cores", TypeSchema::int()),
                        StructField::new(
                            "load_average",
                            TypeSchema::array(TypeSchema::Value(ValueType::Float)),
                        ),
                    ])),
                ),
                // The filesystem the data directory is on.
                StructField::new(
                    "disk",
                    TypeSchema::optional(TypeSchema::struct_of([
                        StructField::new("path", TypeSchema::text()),
                        StructField::new("total_bytes", TypeSchema::int()),
                        StructField::new("available_bytes", TypeSchema::int()),
                    ])),
                ),
            ]),
        ),
    );

    // Exit the process, always, for whatever manages it to start it again
    // (`Restart=on-failure` under systemd, `restart: unless-stopped` under
    // Docker). Nothing is checked first: with no manager, the server stays
    // stopped. The answer comes back before the exit.
    set.register(
        Endpoint::new(
            ids::RESTART_SERVER,
            Method::Post,
            api().lit("system").lit("restart"),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "restarting",
            TypeSchema::bool(),
        )])),
    );
}

/// An external identity provider as the Authentication tab lists it: its
/// name and label, and its settings with the client secret redacted.
fn oauth_provider_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("preset", TypeSchema::text()),
        StructField::new("enabled", TypeSchema::bool()),
        StructField::new("config", TypeSchema::json()),
    ])
}

/// The body that creates or replaces a provider: its settings, per
/// `getOAuthProviderSpec`.
fn oauth_provider_input_schema() -> TypeSchema {
    TypeSchema::struct_of([StructField::new("config", TypeSchema::json())])
}

/// The whole settings screen in one response: what may be set, and what is set.
///
/// `values` is opaque JSON — a bag keyed by the declared settings' names — for
/// the same reason a file store's `config` is: its shape is the declarations',
/// which are data, and a static type could only describe it by freezing it.
/// Secrets in it are the redaction sentinel, never the stored value.
fn settings_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("sections", TypeSchema::array(settings_section_schema())),
        StructField::new("values", TypeSchema::json()),
        // The keys pinned by this host's `feldspar.toml`, which win over the
        // stored values and which a save cannot change.
        StructField::new("host_keys", TypeSchema::array(TypeSchema::text())),
    ])
}

/// One group of settings: its heading, what it is for, and its keys.
fn settings_section_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("fields", TypeSchema::array(settings_field_schema())),
    ])
}

/// A settings key: the same declaration every other configurable thing carries,
/// plus the sentence a settings screen has room to put under the control.
fn settings_field_schema() -> TypeSchema {
    let TypeSchema::Struct(fields) = form_field_schema() else {
        // `form_field_schema` is a struct literal one function away; this arm
        // exists because the type says it might not be, not because it can.
        return form_field_schema();
    };
    TypeSchema::struct_of(
        fields
            .into_iter()
            .chain([StructField::new("help", TypeSchema::text())]),
    )
}
