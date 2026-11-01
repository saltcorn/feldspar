//! The admin API's endpoints: agents (configuration) and their runs.
//!
//! `sc-server`'s `handlers::agents` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // An agent is its own record (§11.2, decision 4): a provider, a model, a
    // system prompt and a list of enabled traits. These endpoints are that
    // row's lifecycle, in the shape the triggers' are — because the two records
    // have the same problem. A stored agent that does not validate is **not in
    // the live set**, will not answer, and is still listed here with its reason,
    // because editing it is the repair.
    //
    // The chat *turn* is deliberately not here: it is a WebSocket (§11.4), and
    // this model describes request/response pairs.

    set.register(
        Endpoint::new("listAgents", Method::Get, api().lit("agents"))
            .output(TypeSchema::array(agent_schema()))
            .mcp(
                "List the configured agents: what each is called, which LLM \
                 provider and model it runs on, its system prompt, the traits it \
                 carries with their configuration, and who may reach it.",
            )
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("createAgent", Method::Post, api().lit("agents"))
            .input(agent_input_schema())
            .output(agent_schema())
            .mcp(
                McpTag::new(
                    "Create an agent. Give it a name, a provider and model, a system \
                 prompt, and the traits it should carry — call `listAgentTraits` \
                 first for what a trait is called and what it must be configured \
                 with.",
                )
                .needs(Grant::Create),
            )
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateAgent",
            Method::Put,
            api().lit("agents").param("id", ValueType::Uuid),
        )
        .input(agent_input_schema())
        .output(agent_schema())
        .mcp(
            McpTag::new(
                "Replace an agent's definition. The whole definition is written, so \
             read it with `listAgents` and send it back changed rather than \
                 sending only the part you meant to alter.",
            )
            .needs(Grant::Edit),
        )
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteAgent",
            Method::Delete,
            api().lit("agents").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .mcp(
            McpTag::new(
                "Delete an agent. Its past runs stay — a run is keyed by the agent's \
                 name and outlives the agent deliberately.",
            )
            .needs(Grant::Drop),
        )
        .auth(AuthRequirement::admin()),
    );

    // The registered traits with the configuration each declares, so the agent
    // form renders a form for a trait it knows nothing about — the same move
    // `listActions` and `listLlmProviderBackends` make. `tool_names` is the one
    // addition: a trait's tools are named from its configuration (§11.2), and an
    // admin about to save two traits whose names would collide is better told
    // what they are called than left to discover it from the refusal.
    set.register(
        Endpoint::new("listAgentTraits", Method::Get, api().lit("agent-traits"))
            .output(TypeSchema::array(agent_trait_info_schema()))
            // Tagged (§13.6) for the reason `listFieldTypes` is: it is the
            // declaration `createAgent` is written against, and it also names
            // the tools each trait will offer, which is what a collision check
            // needs.
            .mcp(
                "List the agent traits this installation registers — the \
                 capabilities an agent can be given — each with the \
                 configuration form it declares and the tools it will offer. \
                 This is what `createAgent` and `updateAgent` configure traits \
                 against.",
            )
            .auth(AuthRequirement::admin()),
    );

    // A chat session **is** a run (§11.4), so the history the chat panel shows
    // and the record a triggered run leaves behind are one list. Runs are keyed
    // by the agent's *name*, which is what `_fd_runs.subject` holds — a run
    // outlives the agent it was of, deliberately.

    set.register(
        Endpoint::new(
            "listRuns",
            Method::Get,
            api().lit("agent-runs").param("agent", ValueType::Text),
        )
        .output(TypeSchema::array(run_summary_schema()))
        .mcp(
            "List one agent's runs, newest first — a chat session and a \
             trigger-driven run are both runs. Runs are keyed by the agent's \
             **name**, not its id. Use this and then `getRun` to find out what an \
             agent actually did.",
        )
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "getRun",
            Method::Get,
            api().lit("runs").param("id", ValueType::Uuid),
        )
        .output(run_schema())
        .mcp(
            "Read one run in full: its state, its messages, every tool call it \
             made and what came back. This is where to look when an agent or a \
             workflow did not do what was expected.",
        )
        .auth(AuthRequirement::admin()),
    );

    // One display part a run's tool showed the person (TODO-agent-skills AS6.3):
    // rows as `data`, an image or a clip as `content_base64`.
    set.register(
        Endpoint::new(
            "getRunPart",
            Method::Get,
            api()
                .lit("runs")
                .param("id", ValueType::Uuid)
                .lit("parts")
                .param("part", ValueType::Uuid),
        )
        .output(run_part_schema())
        .auth(AuthRequirement::admin()),
    );

    // What a coding run changed, as a diff over the store — the run's own
    // changes and those of every session it delegated to (a planned run edits
    // nothing itself). Computed from the runs' change ledgers against what the
    // store holds now, so it works on every store backend, git or not.
    set.register(
        Endpoint::new(
            "getRunDiff",
            Method::Get,
            api().lit("runs").param("id", ValueType::Uuid).lit("diff"),
        )
        .output(run_diff_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteRun",
            Method::Delete,
            api().lit("runs").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );
}

/// The fields of an agent's definition that an admin sets (§11.2).
fn agent_fields() -> Vec<StructField> {
    vec![
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // The `_fd_llm_providers` **name** this agent calls through, not its id:
        // that is what the stored row holds, so that is what round-trips.
        StructField::new("provider", TypeSchema::text()),
        // A model row under the provider, by name. Null means "the provider's
        // default model", which is the common case and a real answer rather
        // than a missing one.
        StructField::new("model", TypeSchema::optional(TypeSchema::text())),
        StructField::new("system_prompt", TypeSchema::text()),
        // A **list** of `{trait, config}` pairs, not a map: a trait may be
        // enabled more than once (§11.2), and the order is the order its tools
        // are offered to the model in.
        StructField::new("traits", TypeSchema::array(enabled_trait_schema())),
        // Null is admin-only, the same safe reading a trigger's takes.
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
        // The sparse per-agent values (§9): `temperature`, `max_tokens`,
        // `max_steps`. A bag rather than three fields, because they are exactly
        // §9's sparse attributes and absent means "the provider's default",
        // which no number could stand in for.
        StructField::new("attributes", TypeSchema::json()),
    ]
}

/// One enabled trait: which trait, and how this instance is configured.
fn enabled_trait_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("trait", TypeSchema::text()),
        StructField::new("config", TypeSchema::json()),
    ])
}

fn agent_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("id", TypeSchema::uuid())];
    fields.extend(agent_fields());
    fields.push(StructField::new(
        "error",
        TypeSchema::optional(TypeSchema::text()),
    ));
    TypeSchema::Struct(fields)
}

/// The body accepted when creating or updating an agent: the definition's fields
/// minus the id (server-assigned on create, taken from the path on update) and
/// minus `error` (the server's answer, not the admin's input).
fn agent_input_schema() -> TypeSchema {
    TypeSchema::Struct(agent_fields())
}

/// A registered agent trait and the configuration it declares, so the agent form
/// renders a form for a trait it has never heard of.
fn agent_trait_info_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
    ])
}

/// One stored display part (TODO-agent-skills AS6.3).
fn run_part_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        // `rows` | `image` | `audio`.
        StructField::new("kind", TypeSchema::text()),
        StructField::new("mime_type", TypeSchema::text()),
        StructField::new("summary", TypeSchema::text()),
        // `{ title, view, columns, rows }` for rows, `{ name }` for a file.
        StructField::new("data", TypeSchema::json()),
        // The bytes of an image or a clip.
        StructField::new("content_base64", TypeSchema::optional(TypeSchema::text())),
    ])
}

/// What a run and its sessions changed: one entry per file-store scope.
fn run_diff_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("run", TypeSchema::uuid()),
        // Every run the diff was made from: this one and its descendants.
        StructField::new("runs", TypeSchema::array(TypeSchema::uuid())),
        StructField::new(
            "scopes",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("store", TypeSchema::text()),
                // The directory within the store the paths are relative to.
                StructField::new("root", TypeSchema::text()),
                StructField::new(
                    "files",
                    TypeSchema::array(TypeSchema::struct_of([
                        StructField::new("path", TypeSchema::text()),
                        // `added` | `modified` | `deleted`.
                        StructField::new("status", TypeSchema::text()),
                        // Null for a file too large, or not text, to diff.
                        StructField::new("added", TypeSchema::optional(TypeSchema::int())),
                        StructField::new("removed", TypeSchema::optional(TypeSchema::int())),
                    ])),
                ),
                StructField::new(
                    "moves",
                    TypeSchema::array(TypeSchema::struct_of([
                        StructField::new("from", TypeSchema::text()),
                        StructField::new("to", TypeSchema::text()),
                    ])),
                ),
                StructField::new("stat", TypeSchema::text()),
                StructField::new("unified", TypeSchema::text()),
            ])),
        ),
    ])
}
