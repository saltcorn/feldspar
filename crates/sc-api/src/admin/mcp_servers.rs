//! The admin API's endpoints: remote MCP servers an agent calls.
//!
//! Their handlers are registered by `sc-server`'s `mcp_servers` module.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    //
    // Remote MCP servers an agent's `mcp_server` trait calls. A header's value
    // is a secret, kept as a provider's API key is: it reads back as the
    // sentinel, and a save that sends the sentinel back keeps what is stored.

    set.register(
        Endpoint::new("listMcpServers", Method::Get, api().lit("mcp-servers"))
            .output(TypeSchema::array(mcp_server_schema()))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("createMcpServer", Method::Post, api().lit("mcp-servers"))
            .input(TypeSchema::Struct(mcp_server_fields()))
            .output(mcp_server_schema())
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateMcpServer",
            Method::Put,
            api().lit("mcp-servers").param("id", ValueType::Uuid),
        )
        .input(TypeSchema::Struct(mcp_server_fields()))
        .output(mcp_server_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteMcpServer",
            Method::Delete,
            api().lit("mcp-servers").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // **Test.** Connects to the server the body describes (a saved one's
    // header sentinels resolved when `id` names it), lists its tools and
    // reports them, or the server's own refusal as `ok: false`: the refusal is
    // the answer to "does this work?".
    set.register(
        Endpoint::new("testMcpServer", Method::Post, api().lit("mcp-server-test"))
            .input(TypeSchema::Struct({
                let mut fields = vec![StructField::new(
                    "id",
                    TypeSchema::optional(TypeSchema::uuid()),
                )];
                fields.extend(mcp_server_fields());
                fields
            }))
            .output(mcp_server_test_schema())
            .auth(AuthRequirement::admin()),
    );

    // A saved server's tools: the cached listing, or a fresh one with
    // `refresh`. A server that cannot be reached is `ok: false`, as for Test.
    set.register(
        Endpoint::new(
            "listMcpServerTools",
            Method::Get,
            api()
                .lit("mcp-servers")
                .param("id", ValueType::Uuid)
                .lit("tools"),
        )
        .query([QueryParam::new("refresh", ValueType::Bool)])
        .output(mcp_server_test_schema())
        .auth(AuthRequirement::admin()),
    );
}

/// The fields of an MCP server an admin sets.
fn mcp_server_fields() -> Vec<StructField> {
    vec![
        // Lowercase letters, digits, `-` and `_`: its tools are offered as
        // `<name>__<tool>`.
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("url", TypeSchema::text()),
        // `streamable_http` (the default) or `sse`.
        StructField::new("transport", TypeSchema::optional(TypeSchema::text())),
        // Sent with every request. **On the way out each value is the
        // sentinel**; sending it back keeps the stored value.
        StructField::new(
            "headers",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("value", TypeSchema::text()),
            ])),
        ),
        StructField::new("allow_private_network", TypeSchema::bool()),
        // Seconds one call may take; 0 is the default (60).
        StructField::new("timeout_secs", TypeSchema::int()),
    ]
}

/// An MCP server as reported to the admin UI, its header values redacted.
fn mcp_server_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("id", TypeSchema::uuid())];
    fields.extend(mcp_server_fields());
    TypeSchema::Struct(fields)
}

/// What testing a server, or listing its tools, answers.
fn mcp_server_test_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("ok", TypeSchema::bool()),
        // The server's refusal on failure; a summary on success.
        StructField::new("message", TypeSchema::text()),
        StructField::new(
            "server",
            TypeSchema::optional(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("version", TypeSchema::text()),
                StructField::new("protocol_version", TypeSchema::text()),
                StructField::new("instructions", TypeSchema::optional(TypeSchema::text())),
                StructField::new("tools_list_changed", TypeSchema::bool()),
            ])),
        ),
        StructField::new(
            "tools",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("title", TypeSchema::optional(TypeSchema::text())),
                StructField::new("description", TypeSchema::text()),
                StructField::new("input_schema", TypeSchema::json()),
                StructField::new("read_only", TypeSchema::bool()),
            ])),
        ),
    ])
}
