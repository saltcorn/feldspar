//! The admin API's endpoints: the admin UI's home page: every entity in one list.
//!
//! Their handlers are registered by `sc-server`'s `entities` module.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // Every entity an admin configures — tables, file stores, triggers, agents
    // and applications — in one list, each with when it was last saved, so the
    // home page can show the recently edited ones and search them all. The
    // per-kind lists stay the place to read one kind in full; this carries what
    // a row of a mixed list shows. With `deep`, each also carries its whole
    // definition as text, for a search that looks inside it (secrets masked,
    // as the per-kind lists mask them).
    set.register(
        Endpoint::new(ids::LIST_ENTITIES, Method::Get, api().lit("entities"))
            .query([QueryParam::new("deep", ValueType::Bool)])
            .output(TypeSchema::array(entity_schema())),
    );
}

/// One stored trigger: what fires it, what it runs, and whether it is usable.
///
/// `error` is the part that makes a broken trigger fixable rather than merely
/// absent, exactly as a file store's is: a trigger whose table was dropped or
/// whose action a removed plugin provided is **not in the live set** and will not
/// fire, but it is still stored, still listed and still editable — and the reason
/// is the only thing that says what to fix.
/// One row of `listEntities`.
fn entity_schema() -> TypeSchema {
    TypeSchema::struct_of([
        // `table`, `file_store`, `trigger`, `stream`, `agent` or `application`.
        StructField::new("kind", TypeSchema::text()),
        // What the entity's own screen is addressed by: a table's name, or the
        // id of any other kind's row.
        StructField::new("id", TypeSchema::text()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // Short facts for the Details column, most telling first: a trigger's
        // event and action, an agent's provider and model, a store's backend.
        StructField::new("details", TypeSchema::array(TypeSchema::text())),
        // Who may use it, as role names: `read/write` for a table.
        StructField::new("access", TypeSchema::text()),
        // Null for one not saved since edit times were recorded.
        StructField::new("updated_at", TypeSchema::optional(TypeSchema::timestamp())),
        // Whether the Run link means anything now: false for a trigger that is
        // switched off or not usable.
        StructField::new("runnable", TypeSchema::bool()),
        // An application's subdomain, for the link that opens it.
        StructField::new("subdomain", TypeSchema::optional(TypeSchema::text())),
        // What a trigger's event is about: the table of a table event, the
        // stream of a stream event. Null for every other entity.
        StructField::new("channel", TypeSchema::optional(TypeSchema::text())),
        // The whole definition as JSON text — only when asked for with `deep`.
        StructField::new("definition", TypeSchema::optional(TypeSchema::text())),
    ])
}
