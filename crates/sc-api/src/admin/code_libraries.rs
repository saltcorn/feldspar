//! The admin API's endpoints: code libraries a code body may import.
//!
//! `sc-server`'s `handlers::code_libraries` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // Named, shared JavaScript that code queries and triggers import with
    // `lib(name)`: instance-wide, or one application's. A save compile-checks
    // the code (without running it). Not projected to MCP here: MCP has its own
    // three tools (`describe_code_libraries`, `save_code_library`,
    // `delete_code_library`), which speak names rather than ids.

    set.register(
        Endpoint::new(
            "listCodeLibraries",
            Method::Get,
            api().lit("code-libraries"),
        )
        .output(TypeSchema::array(code_library_schema()))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "createCodeLibrary",
            Method::Post,
            api().lit("code-libraries"),
        )
        .input(code_library_input_schema())
        .output(code_library_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateCodeLibrary",
            Method::Put,
            api().lit("code-libraries").param("id", ValueType::Uuid),
        )
        .input(code_library_input_schema())
        .output(code_library_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteCodeLibrary",
            Method::Delete,
            api().lit("code-libraries").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );
}

/// One stored agent, with the reason it cannot run when there is one.
///
/// `error` is what makes a broken agent fixable rather than merely absent, as a
/// trigger's and a file store's are: an agent naming a provider that was deleted
/// or a trait configured against a dropped table is **not in the live set** and
/// will not answer, but it is still stored, still listed and still editable.
/// A code library's editable fields (TODO F3): what a create or update sends.
fn code_library_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("code", TypeSchema::text()),
        // Absent or null: instance-wide.
        StructField::new("application_id", TypeSchema::optional(TypeSchema::uuid())),
    ])
}

/// A code library as listed: its fields, its application's subdomain, and the
/// queries and triggers whose code imports it by name.
fn code_library_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("code", TypeSchema::text()),
        StructField::new("application_id", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("application", TypeSchema::optional(TypeSchema::text())),
        StructField::new(
            "used_by",
            TypeSchema::optional(TypeSchema::array(TypeSchema::struct_of([
                StructField::new("kind", TypeSchema::text()),
                StructField::new("name", TypeSchema::text()),
                StructField::new("application", TypeSchema::optional(TypeSchema::text())),
            ]))),
        ),
    ])
}
