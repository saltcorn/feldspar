//! The admin API's endpoints: streams.
//!
//! `sc-server`'s `handlers::streams` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // Dataflows as an entity (TODO "Streams"). A **stream provider** is code
    // that can observe something — MQTT, a module's polled feed — and a
    // **stream** is one of those with its settings filled in, named, and
    // running. Six endpoints, which is the model screens' five plus the one a
    // flow needs that a row does not: the live status, because a stream is the
    // only entity here whose *current* state is not in its row.
    //
    // The Observe socket is **not** here, and cannot be: it is
    // `GET /api/streams/{id}/observe`, mounted beside this set in `router.rs`,
    // for the reason the language server's and the admin chat's are — an
    // `EndpointSet` is a typed request/response model and a socket has no shape
    // in it (§13.1).

    // The providers this build carries, each with the settings it declares —
    // the same "settings as data" move `listModelProviders` makes, so the
    // Streams form renders a provider it has never heard of.
    //
    // `?configuration=` is what turns a declaration into an answer: the element
    // type is a **function of the configuration** (§3), so MQTT with
    // `payload = json` and four declared keys answers a different
    // `element_type` from the same provider with `payload = text`. Without it
    // the answer is the declaration alone, which is what the picker shows
    // before anything has been filled in. Text holding JSON rather than a
    // `Json` query parameter, for `listModelProviders`' reason: a query string
    // carries text, and a generated client would stringify an object to
    // `[object Object]`.
    //
    // An object rather than a bare array, because the empty list is a real
    // state with a sentence attached — a build made with
    // `--no-default-features` has no MQTT — and an empty picker reads like a
    // bug where "this build was made without it" reads like the decision it is.
    set.register(
        Endpoint::new(
            ids::LIST_STREAM_PROVIDERS,
            Method::Get,
            api().lit("stream-providers"),
        )
        .query([
            QueryParam::new("provider", ValueType::Text),
            QueryParam::new("configuration", ValueType::Text),
        ])
        .output(TypeSchema::struct_of([
            StructField::new("providers", TypeSchema::array(stream_provider_schema())),
            StructField::new("builtins_compiled_out", TypeSchema::bool()),
            StructField::new("notice", TypeSchema::optional(TypeSchema::text())),
        ])),
    );

    set.register(
        Endpoint::new(ids::LIST_STREAMS, Method::Get, api().lit("streams"))
            .output(TypeSchema::array(stream_schema())),
    );

    set.register(
        Endpoint::new(
            ids::GET_STREAM,
            Method::Get,
            api().lit("streams").param("id", ValueType::Uuid),
        )
        .output(stream_schema()),
    );

    // One endpoint for create and replace, as `saveModel` is and for its
    // reason: the form always sends the whole definition, so two endpoints
    // would be one behaviour under two names, and the id in the body is what
    // says which.
    //
    // A save **reloads the supervisor**, so the flow follows the row without a
    // restart: a stream saved enabled is connected by the time the response is
    // written, and one whose broker moved has dropped the old session.
    set.register(
        Endpoint::new(ids::SAVE_STREAM, Method::Post, api().lit("streams"))
            .input(stream_input_schema())
            .output(stream_schema()),
    );

    // Refused while a trigger names this stream as its channel, listing them —
    // the refusal `delete_llm_model` already makes, for the same reason: the
    // reference is by name, so deleting the stream would leave a trigger
    // listening to a channel nothing will ever raise.
    set.register(
        Endpoint::new(
            ids::DELETE_STREAM,
            Method::Delete,
            api().lit("streams").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )])),
    );

    // How it is going **right now**: the one endpoint a stream needs that a
    // model does not. A stream's state — connected, retrying since when, how
    // many elements — is held in memory by the supervisor and is deliberately
    // not a column (§6), so it cannot be read back from the row, and the
    // Streams list polls this rather than re-reading definitions it already
    // has.
    set.register(
        Endpoint::new(
            ids::STREAM_STATUS,
            Method::Get,
            api()
                .lit("streams")
                .param("id", ValueType::Uuid)
                .lit("status"),
        )
        .output(stream_status_schema()),
    );
}

/// One stream provider the picker offers, and everything the Streams form needs
/// to render it without knowing what it is.
///
/// [`model_provider_schema`]'s twin, with `outcome` replaced by `element_type`
/// — which is the same idea under the name a flow gives it: what *this
/// configuration* would produce. It is present only when the request carried a
/// configuration that resolves to one, and `element_type_error` is the sentence
/// saying why it does not ("`payload` is `json` but no keys are declared"),
/// which is what the form is waiting to be told.
fn stream_provider_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        // A human name for the picker, which a provider whose registered name
        // is already a word an admin knows (`mqtt`) leaves equal to it.
        StructField::new("label", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // The module supplying it, or null for a built-in — what the picker
        // renders "built in" against.
        StructField::new("module", TypeSchema::optional(TypeSchema::text())),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
        StructField::new("element_type", TypeSchema::optional(TypeSchema::json())),
        StructField::new(
            "element_type_error",
            TypeSchema::optional(TypeSchema::text()),
        ),
    ])
}

/// One stored stream, as the list and the form see it.
///
/// Three things ride along that are **not** in the `_fd_streams` row, and each
/// is a deliberate §5 decision showing through:
///
/// - `element_type` is a pure function of `provider` + `configuration`, so
///   storing a copy would be a second answer that drifts the day a provider's
///   declaration changes. It is computed on read.
/// - `status` and `counters` are the supervisor's, held in memory only (§6):
///   connected or retrying, and what has come through *since this server
///   started*. Null when this server is not running the stream at all.
/// - `error` is the twin of a model's and a trigger's: a stream that stopped
///   validating — a provider whose module was uninstalled, a topic filter that
///   no longer parses — is **still listed and still editable**, because editing
///   it is the repair.
/// - `subscribers` is how many live subscriptions applications' pages hold to it
///   on this server right now (TODO.md "Live updates" L1.11), and `topics` how
///   its provider splits it into topics — both from memory and the provider,
///   like the status.
/// - `changes`, for a `table_changes` stream, is what its per-element checks
///   have decided since this server started (L3.9): `checked`, `delivered`,
///   `suppressed_access`, `suppressed_filter` and `evaluator_micros`. Null for
///   any other stream, and for one nobody has subscribed to here.
///
/// The `configuration` is the **redacted** one: a setting the provider declared
/// secret comes back as the sentinel, never as the password.
fn stream_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("provider", TypeSchema::text()),
        StructField::new("configuration", TypeSchema::json()),
        // The floor for **subscribing** to it from an application's page. Null
        // is admin-only — the trigger rule, for the trigger reason: a flow
        // nobody has thought about the access of is not public.
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
        StructField::new("attributes", TypeSchema::json()),
        // Lifted out of `attributes` for the list's switch, as a trigger's is:
        // it is the one attribute every row has an answer for.
        StructField::new("enabled", TypeSchema::bool()),
        StructField::new("element_type", TypeSchema::optional(TypeSchema::json())),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        StructField::new("status", TypeSchema::optional(TypeSchema::json())),
        StructField::new("counters", TypeSchema::optional(TypeSchema::json())),
        StructField::new("topics", TypeSchema::optional(TypeSchema::json())),
        StructField::new("subscribers", TypeSchema::int()),
        StructField::new("changes", TypeSchema::optional(TypeSchema::json())),
    ])
}

/// What a `saveStream` sends. The id is what says create or replace.
///
/// A secret setting may come back as the sentinel it was handed, and the save
/// puts the stored value behind it (§2.3) — so a password survives an edit that
/// did not retype it.
fn stream_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("provider", TypeSchema::text()),
        StructField::new("configuration", TypeSchema::optional(TypeSchema::json())),
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
        StructField::new("attributes", TypeSchema::optional(TypeSchema::json())),
        // Absent means enabled: a stream an admin has just filled in the broker
        // details of is one they want running, and making them press a second
        // switch to find out whether the details were right would be the wrong
        // default in both directions.
        StructField::new("enabled", TypeSchema::optional(TypeSchema::bool())),
    ])
}

/// How a stream is going right now: the `status` object (`starting`, `running`,
/// `failed` with its error and attempt count, or `stopped`), the counters, and
/// the element type the socket would announce.
///
/// `running: false` is the answer for a stream whose row exists and which this
/// process holds no live subscription for — disabled, failed and retrying, or
/// never reloaded here — and it is a 200 rather than a 404, because "this
/// stream is not running" is exactly what the screen asked. `status` tells the
/// three apart; a stream the supervisor has never held has none at all.
fn stream_status_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("running", TypeSchema::bool()),
        StructField::new("status", TypeSchema::optional(TypeSchema::json())),
        StructField::new("counters", TypeSchema::optional(TypeSchema::json())),
        StructField::new("element_type", TypeSchema::optional(TypeSchema::json())),
        // How many sockets are attached, which is the number that says whether
        // an Observe screen somebody left open is still costing anything.
        StructField::new("listeners", TypeSchema::int()),
        // How many of those are applications' pages: live subscriptions held
        // on `{mount}/live` (TODO.md "Live updates" L1.11).
        StructField::new("subscribers", TypeSchema::int()),
        // For a `table_changes` stream, its per-element checks (L3.9).
        StructField::new("changes", TypeSchema::optional(TypeSchema::json())),
    ])
}
