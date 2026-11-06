//! The admin API's endpoints: secondary database connections.
//!
//! `sc-server`'s `handlers::connections` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // The *other* databases an admin has connected (§5). Same four operations as
    // a file store's, addressed by id for the same reason: the row's identity
    // survives a rename, and the name is what tables are stamped with.
    //
    // Create and update **connect** as well as save, so an admin who typed a bad
    // host is told at the keyboard rather than by an empty table list. A
    // connection that saved but could not connect is still a row, still listed
    // and still editable — editing it is the repair.

    set.register(
        Endpoint::new(
            ids::LIST_DATABASE_CONNECTIONS,
            Method::Get,
            api().lit("db-connections"),
        )
        .output(TypeSchema::array(db_connection_schema())),
    );

    set.register(
        Endpoint::new(
            ids::CREATE_DATABASE_CONNECTION,
            Method::Post,
            api().lit("db-connections"),
        )
        .input(db_connection_input_schema())
        .output(db_connection_schema()),
    );

    set.register(
        Endpoint::new(
            ids::UPDATE_DATABASE_CONNECTION,
            Method::Put,
            api().lit("db-connections").param("id", ValueType::Uuid),
        )
        .input(db_connection_input_schema())
        .output(db_connection_schema()),
    );

    set.register(
        Endpoint::new(
            ids::DELETE_DATABASE_CONNECTION,
            Method::Delete,
            api().lit("db-connections").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )])),
    );

    // Dial what the form currently holds, without saving anything. The
    // configure-scope counterpart of a file store's backend operations, and the
    // thing that makes the form usable: a connection is four boxes that are
    // either all right or silently wrong, and this is how an admin finds out
    // which before committing to a name.
    set.register(
        Endpoint::new(
            ids::TEST_DATABASE_CONNECTION,
            Method::Post,
            api().lit("db-connections").lit("test"),
        )
        .input(db_connection_input_schema())
        .output(TypeSchema::struct_of([
            StructField::new("connected", TypeSchema::bool()),
            StructField::new("error", TypeSchema::optional(TypeSchema::text())),
            StructField::new("tables", TypeSchema::int()),
        ])),
    );
}

/// The fields of a database connection an admin sets (§5).
///
/// `password` is a **secret**: it goes out as the sentinel and comes back as
/// the sentinel when the admin did not touch it, which is what
/// `SECRET_SENTINEL` is for. The Postgres ones are the parts of a connection
/// rather than a URL — see `DbConnectionDef` for why the parts.
///
/// `backend` says which of the two kinds this is, and the last two are the
/// SQLite half: a database file is a **file**, so it is named the way every
/// other file is — a store and a path inside it — rather than as a path into the
/// server's filesystem. The fields the chosen backend does not use are empty
/// rather than absent, because this is one row and one form.
fn db_connection_fields() -> Vec<StructField> {
    vec![
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("backend", TypeSchema::text()),
        StructField::new("host", TypeSchema::text()),
        StructField::new("port", TypeSchema::int()),
        StructField::new("database", TypeSchema::text()),
        StructField::new("username", TypeSchema::text()),
        StructField::new("password", TypeSchema::text()),
        StructField::new("schema", TypeSchema::text()),
        StructField::new("file_store", TypeSchema::text()),
        StructField::new("file_path", TypeSchema::text()),
    ]
}

/// A database connection as reported to the admin UI: its definition, plus the
/// live state only the running server knows.
///
/// `connected` and `error` are here for the reason a file store's are: a
/// definition can be perfectly valid and the host still down, and the UI has to
/// show that state with its reason rather than omitting the connection or
/// pretending it works.
///
/// `tables` and `shadowed` are the two halves of "what did this connection
/// actually contribute". The first is how many of its tables are in the catalog;
/// the second names the ones that are **not**, because a table of that name was
/// already there — the primary database always wins a clash, and an admin who
/// cannot find a table they know exists needs to be told that rather than left
/// to guess.
fn db_connection_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("id", TypeSchema::uuid())];
    fields.extend(db_connection_fields());
    fields.extend([
        StructField::new("connected", TypeSchema::bool()),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        StructField::new("tables", TypeSchema::int()),
        StructField::new("shadowed", TypeSchema::array(TypeSchema::text())),
    ]);
    TypeSchema::Struct(fields)
}

/// The body accepted when creating, updating or testing a database connection:
/// the definition's fields minus the id and minus the live state.
fn db_connection_input_schema() -> TypeSchema {
    TypeSchema::Struct(db_connection_fields())
}
