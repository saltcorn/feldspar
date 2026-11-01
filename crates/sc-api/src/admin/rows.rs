//! The admin API's endpoints: rows: CRUD, row history, restore and undo, and CSV export and import.
//!
//! `sc-server`'s `handlers::rows` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(set: &mut EndpointSet) {
    // Every row endpoint here is `admin()`, and that is **not** governed by a
    // table's `min_role_read`/`min_role_write`. Those rules are the table's
    // *application-facing* access (§7), enforced by an application's REST
    // provider (`sc_api::RestProvider`); this is the admin's own view of the
    // data, reached only by role 1 through the admin SPA. A table an admin
    // opened to role 80 for its application is still admin-only here — the two
    // are different surfaces onto the same rows, and reading this as an
    // oversight would be the mistake.

    // A **page** of rows, filtered and ordered by the same query string an
    // application's REST read takes (`crate::query_string`): the admin's data
    // grid is a spreadsheet over a table of any size, so sorting a column,
    // typing in its filter row and scrolling to row 40,000 each cost one page
    // rather than the table. An absent `limit` is [`ROW_PAGE_CAP`], so the
    // endpoint has a bound even when the caller forgets one.
    set.register(
        Endpoint::new(
            "listRows",
            Method::Get,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("rows"),
        )
        .query(row_read_params())
        .output(TypeSchema::array(TypeSchema::json()))
        .auth(AuthRequirement::admin()),
    );

    // How many rows there are, without reading them. The table page shows this
    // beside the link to the rows themselves, and a page that had to fetch every
    // row to print one number would cost what the data costs.
    set.register(
        Endpoint::new(
            "countRows",
            Method::Get,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("rows")
                .lit("count"),
        )
        // The same filter map `listRows` takes, and for one reason: the grid's
        // scrollbar has to be as long as the rows the filter row leaves. A count
        // that ignored the filters would size the scroller to the whole table
        // and leave the last screen of it empty.
        .query([
            QueryParam::new("search", ValueType::Text),
            QueryParam::new("filter", ValueType::Text).map(),
        ])
        .output(TypeSchema::struct_of([StructField::new(
            "count",
            TypeSchema::int(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "createRow",
            Method::Post,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("rows"),
        )
        .input(TypeSchema::json())
        .output(TypeSchema::json())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateRow",
            Method::Put,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("rows")
                .param("id", ValueType::Text),
        )
        .input(TypeSchema::json())
        .output(TypeSchema::json())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteRow",
            Method::Delete,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("rows")
                .param("id", ValueType::Text),
        )
        .auth(AuthRequirement::admin()),
    );

    // Empty the table: every row, in one statement, keeping the table, its
    // fields and its settings. The table page's "Delete all rows". Answers how
    // many rows went, which is the one thing the admin cannot see afterwards.
    set.register(
        Endpoint::new(
            "deleteAllRows",
            Method::Delete,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("rows"),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::int(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    //
    // Admin-only, like the rows themselves above, and for the same reason: a
    // table whose history is opened to role 80 (`history.min_role`) is opened
    // for the surfaces an application's users reach, and this is not one of
    // them. That rule and the row-by-row read check live in `sc_api::history`,
    // which these handlers call too. Each is a tool in the history area, off
    // unless granted, because history is row data (see
    // `the_surfaces_that_were_left_out_stayed_out`).

    set.register(
        Endpoint::new(
            "getRowHistory",
            Method::Get,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("rows")
                .param("id", ValueType::Text)
                .lit("history"),
        )
        .output(TypeSchema::array(history_entry_schema()))
        .auth(AuthRequirement::admin())
        .mcp(
            McpTag::new(
                "Read the history of one row of a table that keeps row history: \
                 its versions newest first, each with the fields it changed (old \
                 and new value; `removed` marks a field dropped since), who made \
                 the change, when, and from where (`source`: request, import, \
                 workflow, undo, restore, sync, mcp or sql). `id` is the row's \
                 primary key. Secrets show as a placeholder.",
            )
            .in_area(Area::History),
        ),
    );

    set.register(
        Endpoint::new(
            "listDeletedRows",
            Method::Get,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("deleted"),
        )
        .query([
            QueryParam::new("limit", ValueType::Int),
            QueryParam::new("offset", ValueType::Int),
        ])
        .output(TypeSchema::array(deleted_row_schema()))
        .auth(AuthRequirement::admin())
        .mcp(
            McpTag::new(
                "List the rows deleted from a table that keeps row history and not \
                 put back since, newest deletion first: each row as it was when \
                 deleted (`removed` holds values of fields dropped since), and \
                 who deleted it, when.",
            )
            .in_area(Area::History),
        ),
    );

    // The row as it was. `as_of` is a time (RFC 3339, or a date) or an entry's
    // `seq` from the row's history; `row` is null when it did not exist then.
    set.register(
        Endpoint::new(
            "getRowAsOf",
            Method::Get,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("rows")
                .param("id", ValueType::Text),
        )
        .query([QueryParam::new("as_of", ValueType::Text).required()])
        .output(row_as_of_schema())
        .auth(AuthRequirement::admin())
        .mcp(
            McpTag::new(
                "Rebuild one row of a table that keeps row history as it was at \
                 `as_of`: a time (like 2026-10-22T09:30:00Z, or a date) or the \
                 `seq` of an entry in the row's history, meaning just after it. \
                 `row` is null when the row did not exist then; `removed` holds \
                 its values of fields dropped since.",
            )
            .in_area(Area::History),
        ),
    );

    set.register(
        Endpoint::new("listChangesets", Method::Get, api().lit("changesets"))
            .query([
                QueryParam::new("user", ValueType::Text),
                QueryParam::new("table", ValueType::Text),
                QueryParam::new("source", ValueType::Text),
                QueryParam::new("since", ValueType::Text),
                QueryParam::new("limit", ValueType::Int),
                QueryParam::new("offset", ValueType::Int),
            ])
            .output(TypeSchema::array(changeset_summary_schema()))
            .auth(AuthRequirement::admin())
            .mcp(
                McpTag::new(
                    "List changesets, newest first: each is one user action (a \
                     request, an import, a workflow run, an MCP call, an offline \
                     upload, a psql transaction…) with how many rows it changed in \
                     which tables that keep history. Filter by `user` (id), \
                     `table`, `source` and `since` (a time).",
                )
                .in_area(Area::History),
            ),
    );

    set.register(
        Endpoint::new(
            "getChangeset",
            Method::Get,
            api().lit("changesets").param("id", ValueType::Text),
        )
        .output(changeset_detail_schema())
        .auth(AuthRequirement::admin())
        .mcp(
            McpTag::new(
                "Read one changeset: who did it, when and from where, and every row \
                 it changed in tables that keep history, with the fields' old and \
                 new values.",
            )
            .in_area(Area::History),
        ),
    );

    //
    // Writes through the row layer as the caller, each in one transaction and
    // its own changeset (`restore` or `undo`), which is recorded like any other
    // change and so can be undone in turn. The logic is `sc_api::undo`.

    set.register(
        Endpoint::new(
            "restoreRow",
            Method::Post,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("deleted")
                .param("id", ValueType::Text)
                .lit("restore"),
        )
        .output(restored_row_schema())
        .auth(AuthRequirement::admin())
        .mcp(
            McpTag::new(
                "Put a deleted row back, as it was when it was deleted, with the same \
                 primary key `id`. Fields dropped from the table since are left out \
                 and listed in `dropped`. Refused when the row refers to a row that \
                 no longer exists (restore that first). Triggers run as for any \
                 insert.",
            )
            .in_area(Area::History)
            .needs(Grant::Edit),
        ),
    );

    set.register(
        Endpoint::new(
            "revertRow",
            Method::Post,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("rows")
                .param("id", ValueType::Text)
                .lit("revert"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "version",
            TypeSchema::int(),
        )]))
        .output(restored_row_schema())
        .auth(AuthRequirement::admin())
        .mcp(
            McpTag::new(
                "Set one row back to how it was at one of its versions (the \
                 `version` number from its history), as one update, or an insert \
                 when the row has been deleted since. Fields dropped since are left \
                 out and listed in `dropped`.",
            )
            .in_area(Area::History)
            .needs(Grant::Edit),
        ),
    );

    set.register(
        Endpoint::new(
            "undoChangeset",
            Method::Post,
            api()
                .lit("changesets")
                .param("id", ValueType::Text)
                .lit("undo"),
        )
        .input(TypeSchema::struct_of([
            // Undo only this table's entries; absent, all of them.
            StructField::new("table", TypeSchema::optional(TypeSchema::text())),
            // Overwrite rows changed since instead of refusing.
            StructField::new("force", TypeSchema::optional(TypeSchema::bool())),
        ]))
        .output(undo_outcome_schema())
        .auth(AuthRequirement::admin())
        .mcp(
            McpTag::new(
                "Undo a changeset: reverse what it did to every row (or only to \
                 `table`'s rows), in one transaction. Inserted rows are deleted, \
                 deleted rows put back and updated fields set back. If any of those \
                 rows has been changed since, nothing is done and `conflicts` says \
                 which rows and fields; pass `force: true` to overwrite them. The \
                 undo is a changeset itself, so it can be undone. Effects outside \
                 the database, such as emails a trigger sent, are not undone.",
            )
            .in_area(Area::History)
            .needs(Grant::Edit)
            .and_needs(Grant::Drop),
        ),
    );

    //
    // The document crosses as a **string** in a JSON envelope rather than as a
    // file download and a file upload. Both directions could have been raw
    // routes outside this set — as the binary file upload is — but neither has
    // to be: CSV is text, so it fits the endpoint model exactly, and keeping it
    // inside means the typed client carries both and the CSRF and auth
    // machinery applies without a second path to remember. The export names the
    // file it should be saved as, because the browser is the one doing the
    // saving and only the server knows the table.

    set.register(
        Endpoint::new(
            "exportTableCsv",
            Method::Get,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("csv"),
        )
        .output(TypeSchema::struct_of([
            StructField::new("filename", TypeSchema::text()),
            StructField::new("csv", TypeSchema::text()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Every row that parses and validates is written; the rest come back as
    // messages naming their line. The three numbers are the answer — "it worked"
    // and "it failed" are both wrong for a file with three bad rows in it, and a
    // file naming primary keys **replaces** rows as well as adding them, which
    // an admin must be told apart from having added them all over again.
    set.register(
        Endpoint::new(
            "importTableCsv",
            Method::Post,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("csv"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "csv",
            TypeSchema::text(),
        )]))
        .output(TypeSchema::struct_of([
            StructField::new("inserted", TypeSchema::int()),
            StructField::new("updated", TypeSchema::int()),
            StructField::new("errors", TypeSchema::array(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );
}

/// The query string a row read takes: the shared vocabulary
/// (`crate::query_string`) minus `select`, which is the REST provider's own
/// shape and has no meaning for a grid that shows the table's own columns.
fn row_read_params() -> Vec<QueryParam> {
    vec![
        QueryParam::new("order", ValueType::Text),
        QueryParam::new("limit", ValueType::Int),
        QueryParam::new("offset", ValueType::Int),
        QueryParam::new("search", ValueType::Text),
        QueryParam::new("filter", ValueType::Text).map(),
    ]
}

/// A changeset as history shows it (§14.12): who, when, from where.
fn changeset_fields() -> Vec<StructField> {
    let text = || TypeSchema::optional(TypeSchema::text());
    vec![
        StructField::new("id", TypeSchema::text()),
        StructField::new("at", TypeSchema::text()),
        StructField::new("user_id", text()),
        // For the admin only: below it, a reader sees who by id.
        StructField::new("user_email", text()),
        StructField::new("source", TypeSchema::text()),
        StructField::new("application", text()),
        StructField::new("description", text()),
        StructField::new("cause", text()),
        StructField::new("undo_table", text()),
        StructField::new("client_id", text()),
        StructField::new("mutation", TypeSchema::optional(TypeSchema::int())),
    ]
}

/// One entry of a row's history: one changeset's net change to the row, field
/// by field.
fn history_entry_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("seq", TypeSchema::int()),
        // The row's version this made, from 1; 0 in a changeset's entries.
        StructField::new("version", TypeSchema::int()),
        StructField::new("table", TypeSchema::text()),
        StructField::new("key", TypeSchema::text()),
        // `i`, `u` or `d`.
        StructField::new("op", TypeSchema::text()),
        StructField::new("at", TypeSchema::text()),
        StructField::new(
            "changes",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("field", TypeSchema::text()),
                StructField::new("old", TypeSchema::json()),
                StructField::new("new", TypeSchema::json()),
                // The field has been dropped from the table since.
                StructField::new("removed", TypeSchema::bool()),
            ])),
        ),
        StructField::new("changeset", TypeSchema::Struct(changeset_fields())),
    ])
}

/// A deleted row: its image, split by whether each field still exists.
fn deleted_row_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("key", TypeSchema::text()),
        StructField::new("seq", TypeSchema::int()),
        StructField::new("at", TypeSchema::text()),
        StructField::new("row", TypeSchema::json()),
        StructField::new("removed", TypeSchema::json()),
        StructField::new("changeset", TypeSchema::Struct(changeset_fields())),
    ])
}

fn row_as_of_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("row", TypeSchema::optional(TypeSchema::json())),
        StructField::new("removed", TypeSchema::json()),
    ])
}

fn changeset_summary_schema() -> TypeSchema {
    let mut fields = changeset_fields();
    fields.push(StructField::new("entries", TypeSchema::int()));
    fields.push(StructField::new(
        "tables",
        TypeSchema::array(TypeSchema::text()),
    ));
    TypeSchema::Struct(fields)
}

/// A restored or reverted row, and the `restore` changeset it was written in.
fn restored_row_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("changeset", TypeSchema::text()),
        StructField::new("row", TypeSchema::json()),
        // Fields the old row had that the table no longer has.
        StructField::new("dropped", TypeSchema::array(TypeSchema::text())),
    ])
}

/// What an undo did, or the conflicts that stopped it.
fn undo_outcome_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("undone", TypeSchema::bool()),
        // The undo's own changeset; null when refused.
        StructField::new("changeset", TypeSchema::optional(TypeSchema::text())),
        StructField::new("entries", TypeSchema::int()),
        // `table.field` of values left out because the field is gone.
        StructField::new("dropped", TypeSchema::array(TypeSchema::text())),
        StructField::new(
            "conflicts",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("table", TypeSchema::text()),
                StructField::new("key", TypeSchema::text()),
                // Null when the row itself is gone, or back.
                StructField::new("field", TypeSchema::optional(TypeSchema::text())),
                StructField::new("expected", TypeSchema::json()),
                StructField::new("actual", TypeSchema::json()),
                StructField::new("reason", TypeSchema::text()),
            ])),
        ),
    ])
}

fn changeset_detail_schema() -> TypeSchema {
    let mut fields = changeset_fields();
    fields.push(StructField::new(
        "entries",
        TypeSchema::array(history_entry_schema()),
    ));
    TypeSchema::Struct(fields)
}
