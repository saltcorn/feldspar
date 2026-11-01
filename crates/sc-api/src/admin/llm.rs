//! The admin API's endpoints: lLM providers and their models.
//!
//! `sc-server`'s `handlers::llm` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // A provider, like a file store, exists only as its stored row (§9, §11.1),
    // and these endpoints are that row's lifecycle. The shape is deliberately
    // the file stores' shape one crate over: id-addressed configuration, a
    // backends endpoint carrying each backend's declared settings so the form is
    // generic. Its models are rows of their own, with their own endpoints below.
    //
    // The one thing that is *not* a copy is what the config carries. A provider's
    // config holds an API key, so every response redacts it
    // (`sc_types::redact_attrs`) and every save merges the sentinel back
    // (`merge_secrets`). That happens where the record is serialised, in the
    // handler, rather than in the screen — see §11.1.

    set.register(
        Endpoint::new("listLlmProviders", Method::Get, api().lit("llm-providers"))
            .output(TypeSchema::array(llm_provider_schema()))
            // Read-only, and its keys come back as the secret sentinel: an
            // agent setting up another agent needs the provider and model
            // names `createAgent` takes, and none of the credentials.
            .mcp(
                "List the LLM providers: each one's name and id (what an agent's \
                 `provider` names), backend and settings, with API keys masked. \
                 Read the models of one with `listLlmModels`. Providers are set up \
                 by an admin in the admin UI, not over this API.",
            )
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "createLlmProvider",
            Method::Post,
            api().lit("llm-providers"),
        )
        .input(llm_provider_input_schema())
        .output(llm_provider_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateLlmProvider",
            Method::Put,
            api().lit("llm-providers").param("id", ValueType::Uuid),
        )
        .input(llm_provider_input_schema())
        .output(llm_provider_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteLlmProvider",
            Method::Delete,
            api().lit("llm-providers").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Make a provider the installation's default: the one used where nothing
    // names a provider (a new application's builder agent, a translation). The
    // provider the configuration file supplies may be made the default like any
    // other — choosing it is using it, not changing it.
    set.register(
        Endpoint::new(
            "setDefaultLlmProvider",
            Method::Put,
            api()
                .lit("llm-providers")
                .param("id", ValueType::Uuid)
                .lit("default"),
        )
        .output(llm_provider_schema())
        .auth(AuthRequirement::admin()),
    );

    // The registered backends with their settings spec, so the create/edit form
    // renders controls for a backend it knows nothing about — the same move
    // `listFileStoreBackends` and `listFrameworks` make.
    set.register(
        Endpoint::new(
            "listLlmProviderBackends",
            Method::Get,
            api().lit("llm-provider-backends"),
        )
        .output(TypeSchema::array(llm_backend_info_schema()))
        .auth(AuthRequirement::admin()),
    );

    // One row per model a provider serves (TODO §3a): the name sent on the wire,
    // whether it is the provider's default, and the model's own settings
    // (prices, context window, capability overrides), declared per backend.
    // Blank settings mean the built-in defaults, so a listed model carries
    // both what is stored and what that resolves to.

    set.register(
        Endpoint::new(
            "listLlmModels",
            Method::Get,
            api()
                .lit("llm-providers")
                .param("id", ValueType::Uuid)
                .lit("models"),
        )
        .output(TypeSchema::array(llm_model_schema()))
        .mcp(
            "List a provider's models: each one's name (what an agent's `model` \
             names), kind (chat, embedding, image, speech), whether it is the \
             provider's default, and its settings — prices, context window, \
             reasoning effort.",
        )
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "createLlmModel",
            Method::Post,
            api()
                .lit("llm-providers")
                .param("id", ValueType::Uuid)
                .lit("models"),
        )
        .input(llm_model_input_schema())
        .output(llm_model_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateLlmModel",
            Method::Put,
            api().lit("llm-models").param("id", ValueType::Uuid),
        )
        .input(llm_model_input_schema())
        .output(llm_model_schema())
        .auth(AuthRequirement::admin()),
    );

    // Refused while an agent calls the model — by name, or as its provider's
    // default — and the refusal names the agents.
    set.register(
        Endpoint::new(
            "deleteLlmModel",
            Method::Delete,
            api().lit("llm-models").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // The model settings a backend declares for a kind of model (chat when
    // `kind` is left out), so the model form renders controls for a backend
    // it knows nothing about. A kind the backend does not serve is refused.
    set.register(
        Endpoint::new(
            "listLlmModelSettings",
            Method::Get,
            api()
                .lit("llm-provider-backends")
                .param("backend", ValueType::Text)
                .lit("model-settings"),
        )
        .query([QueryParam::new("kind", ValueType::Text)])
        .output(TypeSchema::array(form_field_schema()))
        .auth(AuthRequirement::admin()),
    );

    // **Fetch models.** Asks the provider's host which models it serves and
    // answers the names that have no row yet. A host with no listing is an
    // `ok: false` with a message saying to type the name, not an error: it is
    // the answer to the question.
    set.register(
        Endpoint::new(
            "fetchLlmModels",
            Method::Post,
            api()
                .lit("llm-providers")
                .param("id", ValueType::Uuid)
                .lit("fetch-models"),
        )
        .output(TypeSchema::struct_of([
            StructField::new("ok", TypeSchema::bool()),
            StructField::new("message", TypeSchema::text()),
            StructField::new("names", TypeSchema::array(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // **Test.** Sends one trivial prompt to one model through one key, and
    // reports what came back — or the provider's own error text — with the
    // capabilities and prices the model resolved to. It exists because a wrong
    // key or model name is otherwise discovered inside a chat transcript.
    //
    // It takes the provider's config and the model's in the body rather than
    // working from saved rows, so a model can be tested before it is saved. A
    // submitted secret sentinel still resolves against the stored provider when
    // `provider_id` names one, so testing does not require retyping the key.
    set.register(
        Endpoint::new("testLlmModel", Method::Post, api().lit("llm-model-test"))
            .input(TypeSchema::struct_of([
                StructField::new("provider_id", TypeSchema::optional(TypeSchema::uuid())),
                StructField::new("backend", TypeSchema::text()),
                StructField::new("config", TypeSchema::json()),
                StructField::new("name", TypeSchema::text()),
                StructField::new("model_config", TypeSchema::optional(TypeSchema::json())),
                // The model's kind; left out is chat.
                StructField::new("kind", TypeSchema::optional(TypeSchema::text())),
            ]))
            .output(TypeSchema::struct_of([
                StructField::new("ok", TypeSchema::bool()),
                // The model's reply on success, the provider's own words on
                // failure. One field because the admin reads one thing either
                // way: "did this work, and what did it say".
                StructField::new("message", TypeSchema::text()),
                StructField::new("model", TypeSchema::text()),
                StructField::new("capabilities", TypeSchema::json()),
                StructField::new("prices", TypeSchema::json()),
            ]))
            .auth(AuthRequirement::admin()),
    );
}

/// The fields of an LLM provider's definition that an admin sets.
///
/// Shorter than a file store's by one: there is no `min_role`, because a
/// provider is reached only through an agent and it is the agent that carries
/// who may chat with it (§11.2). A floor here as well would be a second
/// authority over the same question.
fn llm_provider_fields() -> Vec<StructField> {
    vec![
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("backend", TypeSchema::text()),
        // The backend's settings. **On the way out this is redacted**: a
        // `secret` setting reads back as the sentinel, never as the key.
        StructField::new("config", TypeSchema::json()),
    ]
}

/// An LLM provider as reported to the admin UI.
///
/// `id` is not optional here, unlike a file store's: even the provider the
/// configuration file supplies, which has no row, has an id derived from its
/// name. That one is `from_config_file`, and neither it nor its models can be
/// edited or deleted. `is_default` marks the installation's default provider —
/// exactly one in a non-empty listing.
///
/// There is no `connected` either, and its absence is the design: connecting a
/// provider builds an HTTP client and sends nothing, so "connected" would be a
/// word for "the configuration parsed" — which the admin already knows, because
/// the save succeeded. Whether a model *works* is a request, and that is what
/// `testLlmModel` is.
fn llm_provider_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("id", TypeSchema::uuid())];
    fields.extend(llm_provider_fields());
    fields.extend([
        StructField::new("from_config_file", TypeSchema::bool()),
        StructField::new("is_default", TypeSchema::bool()),
    ]);
    TypeSchema::Struct(fields)
}

/// The body accepted when creating or updating a provider: the definition's
/// fields minus the id (server-assigned on create, taken from the path on
/// update).
fn llm_provider_input_schema() -> TypeSchema {
    TypeSchema::Struct(llm_provider_fields())
}

/// The fields of a model row that an admin sets.
fn llm_model_fields() -> Vec<StructField> {
    vec![
        // The vendor's model id, as sent on the wire.
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // `chat`, `embedding`, `image` or `speech` (TODO-agent-skills AS3.1);
        // left out on create is chat. Fixed once saved.
        StructField::new("kind", TypeSchema::optional(TypeSchema::text())),
        // At most one per provider and kind: saving one as the default clears
        // the rest of its kind.
        StructField::new("is_default", TypeSchema::bool()),
        // The model's settings, per the backend's `listLlmModelSettings`.
        // Blank settings are dropped on save and mean the built-in default.
        StructField::new("config", TypeSchema::json()),
    ]
}

/// A model row as reported to the admin UI: the row, plus what its settings
/// resolve to, so the form can show the built-in default beside a blank.
fn llm_model_schema() -> TypeSchema {
    let mut fields = vec![
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("provider_id", TypeSchema::uuid()),
    ];
    fields.extend(llm_model_fields());
    fields.push(StructField::new("capabilities", TypeSchema::json()));
    fields.push(StructField::new("prices", TypeSchema::json()));
    TypeSchema::Struct(fields)
}

/// The body accepted when creating or updating a model: the row's fields minus
/// its ids (the provider comes from the path on create, and a model never moves
/// to another provider).
fn llm_model_input_schema() -> TypeSchema {
    TypeSchema::Struct(llm_model_fields())
}

/// A registered LLM provider backend and the settings it declares.
///
/// No `operations`: nothing an LLM provider offers is an *act* on its own
/// configuration the way a git store's clone is. Testing the connection is one
/// endpoint rather than a declared operation because it is the same act for
/// every backend — there is no per-backend list for the UI to render.
fn llm_backend_info_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
    ])
}
