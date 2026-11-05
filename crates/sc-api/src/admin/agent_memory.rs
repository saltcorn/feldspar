//! The admin API's endpoints: what agents remember.
//!
//! Their handlers are registered by `sc-server`'s `agent_memory` module.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    //
    // What agents' `memory` traits stored, personal and shared, for the admin
    // to read and delete. Every user's memories, which is why it is admin-only:
    // a personal memory is otherwise only ever read back for its own user.

    set.register(
        Endpoint::new(
            ids::LIST_AGENT_MEMORIES,
            Method::Get,
            api().lit("agent-memories"),
        )
        .query([
            QueryParam::new("agent", ValueType::Text),
            QueryParam::new("user", ValueType::Text),
            QueryParam::new("kind", ValueType::Text),
            QueryParam::new("search", ValueType::Text),
            QueryParam::new("limit", ValueType::Int),
        ])
        .output(TypeSchema::array(agent_memory_schema())),
    );

    set.register(
        Endpoint::new(
            ids::DELETE_AGENT_MEMORY,
            Method::Delete,
            api().lit("agent-memories").param("id", ValueType::Int),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )])),
    );
}

/// One stored agent memory (TODO-agent-skills AS8.3).
fn agent_memory_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::int()),
        StructField::new("agent", TypeSchema::text()),
        StructField::new("user_id", TypeSchema::optional(TypeSchema::text())),
        StructField::new("run_id", TypeSchema::optional(TypeSchema::text())),
        StructField::new("written_at", TypeSchema::text()),
        StructField::new("kind", TypeSchema::text()),
        StructField::new("personal", TypeSchema::bool()),
        StructField::new("topic", TypeSchema::text()),
        StructField::new("contents", TypeSchema::text()),
    ])
}
