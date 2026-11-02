//! The admin API's endpoints: tables: listing, creating (from scratch, a CSV or a geo file),
//! renaming and dropping them, the metadata tables, provided tables and orphaned table settings.
//!
//! `sc-server`'s `handlers::tables` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(set: &mut EndpointSet) {
    set.register(
        Endpoint::new("listTables", Method::Get, api().lit("tables"))
            .output(TypeSchema::array(table_schema()))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("createTable", Method::Post, api().lit("tables"))
            .input(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                // Which database to create it in (§5.0). Optional, and empty
                // means Saltcorn's own: an installation with no connections
                // never has this question, and a client written before
                // connections existed keeps working unchanged.
                StructField::new("database", TypeSchema::optional(TypeSchema::text())),
            ]))
            .output(table_schema())
            .auth(AuthRequirement::admin()),
    );

    // Create a table **from a CSV file**: the fields deduced from the header and
    // the values under it, then every row imported (§13.1). A separate endpoint
    // rather than an optional `csv` on `createTable`, because it is a different
    // operation with a different failure mode — this one can fail *after* the
    // table exists, and answers by dropping it, which is not something the plain
    // create can do.
    set.register(
        Endpoint::new(
            "createTableFromCsv",
            Method::Post,
            api().lit("tables").lit("csv"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("name", TypeSchema::text()),
            StructField::new("csv", TypeSchema::text()),
            // As for `createTable`: optional, empty means the primary.
            StructField::new("database", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("table", table_schema()),
            StructField::new("inserted", TypeSchema::int()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Create a table **from a geographic file** (analytics TODO A5.2): GeoJSON,
    // a zipped Shapefile or a GeoPackage, its geometry reprojected to WGS84 by
    // PostGIS. The file travels as base64 because two of the three are binary;
    // `layer` picks one of several Shapefiles in a zip or tables in a package.
    set.register(
        Endpoint::new(
            "createTableFromGeoFile",
            Method::Post,
            api().lit("tables").lit("geo"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("name", TypeSchema::text()),
            StructField::new("file_name", TypeSchema::text()),
            StructField::new("content_base64", TypeSchema::text()),
            StructField::new("layer", TypeSchema::optional(TypeSchema::text())),
            StructField::new("database", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("table", table_schema()),
            StructField::new("inserted", TypeSchema::int()),
            StructField::new("warnings", TypeSchema::array(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Set a table's configuration: the `_fd_tables` overlay fields, and only
    // those (§9). A `PUT` on the table's own path rather than a nested
    // `…/settings` resource, because from the admin's side there is one table
    // with settings, not a table plus a settings object hanging off it — and
    // renaming, the other thing a `PUT` on a table might mean, is a schema
    // change with references to chase and is deliberately not offered.
    set.register(
        Endpoint::new(
            "updateTable",
            Method::Put,
            api().lit("tables").param("table", ValueType::Text),
        )
        .input(table_settings_schema())
        .output(table_schema())
        .auth(AuthRequirement::admin()),
    );

    // Drop a table: its columns, its rows and its overlay row. Distinct from
    // `deleteTableSettings`, which forgets a *configuration* and leaves the table
    // exactly where it was — the two verbs are a `DELETE` apart on purpose, and
    // the settings one carries the `/settings` suffix because it is the narrower
    // of the pair.
    //
    // It exists because an agent can now do this (§11.3), and an agent must not
    // be able to do something the admin UI cannot.
    set.register(
        Endpoint::new(
            "dropTable",
            Method::Delete,
            api().lit("tables").param("table", ValueType::Text),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "dropped",
            TypeSchema::text(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Rename a table: the table page's "Rename table". The rows, fields and
    // settings stay; what names the table — keys, views, triggers, an
    // application's subset — follows (`schema_edit::Operation::RenameTable`).
    // Answers the table under its new name, and the notes the rename made:
    // chiefly that code naming the old name was not rewritten.
    set.register(
        Endpoint::new(
            "renameTable",
            Method::Post,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("rename"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "name",
            TypeSchema::text(),
        )]))
        .output(TypeSchema::struct_of([
            StructField::new("table", table_schema()),
            StructField::new("notes", TypeSchema::array(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    //
    // One of Saltcorn's own `_fd_*` tables, added to the tables list so that its
    // rows and settings can be edited — never its schema. Listing what could be
    // added, and adding one; removing one is `dropTable`, which forgets the row
    // that put it in the list rather than dropping anything.

    set.register(
        Endpoint::new(
            "listMetadataTables",
            Method::Get,
            api().lit("metadata-tables"),
        )
        .output(TypeSchema::array(TypeSchema::text()))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "createMetadataTable",
            Method::Post,
            api().lit("tables").lit("metadata"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "name",
            TypeSchema::text(),
        )]))
        .output(table_schema())
        .auth(AuthRequirement::admin()),
    );

    //
    // A **provided** table is one whose rows come from a module rather than from
    // a database: `@saltcorn/rss`'s `RSS feed`, `@saltcorn/proxmox`'s cluster
    // listings. Three endpoints, and the shape of them is the whole design in
    // miniature — nothing here issues DDL, because there is no table in any
    // database to issue it against:
    //
    // - listing what is available, which is a property of the *installed
    //   modules* and not of any table;
    // - creating one, which writes a definition row;
    // - configuring one, which rewrites that row's configuration and may change
    //   the table's columns, because the columns are the module's answer.
    //
    // Dropping one is `dropTable`, which reads the table and forgets the
    // definition instead of dropping a table nothing has.

    set.register(
        Endpoint::new(
            "listTableProviders",
            Method::Get,
            api().lit("table-providers"),
        )
        .output(TypeSchema::array(table_provider_schema()))
        // Tagged (§13.6): `edit_schema` can create a provided table and the
        // provider's key and configuration spec are the only way to know what
        // one may be called and what it needs. Without this the tool is a form
        // with no field list.
        .mcp(
            "List the registered table providers — the modules that supply a \
             table's rows from somewhere other than this database (a REST API, a \
             spreadsheet, a view over other tables). Each carries the \
             configuration it needs, which is what `edit_schema`'s \
             `create_provided_table` operation must be given.",
        )
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "createProvidedTable",
            Method::Post,
            api().lit("tables").lit("provided"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("name", TypeSchema::text()),
            StructField::new("module", TypeSchema::text()),
            StructField::new("provider", TypeSchema::text()),
            // What the provider's own configuration form was filled in with.
            // Optional because a provider may ask for nothing, and because the
            // dialog may create the table first and configure it after.
            StructField::new("configuration", TypeSchema::optional(TypeSchema::json())),
        ]))
        .output(table_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateProvidedTable",
            Method::Put,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("provider"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "configuration",
            TypeSchema::json(),
        )]))
        .output(table_schema())
        .auth(AuthRequirement::admin()),
    );

    // Forget a table's configuration, returning it to the closed default. Also
    // the way an *orphan* row — one whose table is gone (§1.1) — is cleaned up,
    // which is why the path is addressed by name and does not require the table
    // to exist.
    set.register(
        Endpoint::new(
            "deleteTableSettings",
            Method::Delete,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("settings"),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Stored settings whose table is not in the database. These are deliberately
    // kept rather than deleted (§1.1) — a restore or an external migration can
    // drop and recreate a table, and the configuration must be waiting when it
    // returns — so something has to be able to *show* them, or "kept" becomes
    // "invisible and inexplicable".
    set.register(
        Endpoint::new(
            "listOrphanTableSettings",
            Method::Get,
            api().lit("table-settings").lit("orphans"),
        )
        .output(TypeSchema::array(orphan_table_settings_schema()))
        .auth(AuthRequirement::admin()),
    );
}

/// A table in the catalog: its name, plus the `_fd_tables` overlay merged onto
/// it (§9).
///
/// The access roles are in the *list* response, not only in a detail one,
/// because "which of these tables can the public read?" is a question an admin
/// asks about the whole set and should not have to open eight screens to answer.
///
/// `configured` distinguishes a table an admin has set to admin-only from one
/// nobody has touched — both read `1`/`1`, and only the first has a row to
/// delete. Without it the UI could not offer "forget these settings" honestly.
fn table_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("name", TypeSchema::text())];
    fields.extend(table_settings_fields());
    fields.push(StructField::new("configured", TypeSchema::bool()));
    // Why the stored ownership formula is not in effect, when it is not — a
    // stored formula can stop validating when the schema changes under it
    // (fail closed, §7.3), and the admin fixes it where they typed it.
    fields.push(StructField::new(
        "ownership_error",
        TypeSchema::optional(TypeSchema::text()),
    ));
    // Whether the backend can enforce RLS at all (`DbCapabilities`). The SPA
    // renders the RLS toggle only when this is true — a toggle that can only
    // ever be refused is not a setting, it is a trap.
    fields.push(StructField::new("rls_available", TypeSchema::bool()));
    // Whether offline sync can be switched on: a one-column primary key, in
    // Saltcorn's own Postgres database. The form shows the switch only then.
    fields.push(StructField::new("sync_available", TypeSchema::bool()));
    // Whether row history can be switched on: the same conditions as sync's
    // (§14.12), reported apart so neither form has to know that.
    fields.push(StructField::new("history_available", TypeSchema::bool()));
    // Which database hosts the table: `primary`, or the name of the connection
    // it came from (§5's Connections). Reported on every table rather than only
    // on foreign ones, so a client never has to read absence as "the primary".
    fields.push(StructField::new("database", TypeSchema::text()));
    // Whether this is one of Saltcorn's own metadata tables, added to the list:
    // its rows and settings are editable, its fields and constraints are not.
    fields.push(StructField::new("metadata", TypeSchema::bool()));
    // The installed module that declared this table (TODO-imap M4.3), or null,
    // and the fields it declared: the list badges the table, and the field
    // list refuses to delete those fields while the module is installed.
    fields.push(StructField::new(
        "module",
        TypeSchema::optional(TypeSchema::text()),
    ));
    fields.push(StructField::new(
        "module_fields",
        TypeSchema::array(TypeSchema::text()),
    ));
    // The table provider serving its rows, or null for a table in a database
    // (§8.3). Present on every table rather than only on provided ones, so a
    // client never has to read absence as "it is in a database".
    fields.push(StructField::new(
        "provider",
        TypeSchema::optional(TypeSchema::struct_of([
            StructField::new("module", TypeSchema::text()),
            StructField::new("provider", TypeSchema::text()),
            // What the admin filled in, and what to fill in — the values and the
            // declaration together, because the settings form on the table's own
            // page needs both and one round trip is what it has.
            StructField::new("configuration", TypeSchema::json()),
            StructField::new("config_spec", TypeSchema::array(form_field_schema())),
            // Which of v1's three write methods `get_table` answers for these
            // settings (§8.3). Reported because writability is a property of
            // the *configuration* — the same provider, configured read-only,
            // answers none of them — so a client cannot infer it from the
            // provider's name, and a button with nothing behind it is a screen
            // that lies.
            StructField::new(
                "writes",
                TypeSchema::struct_of([
                    StructField::new("insert", TypeSchema::bool()),
                    StructField::new("update", TypeSchema::bool()),
                    StructField::new("delete", TypeSchema::bool()),
                ]),
            ),
            // Why this table has no columns, when it has none: the module is not
            // installed, the provider is not one it supplies, `fields(cfg)`
            // threw. Empty when all is well.
            StructField::new("issues", TypeSchema::array(TypeSchema::text())),
        ])),
    ));
    TypeSchema::Struct(fields)
}

/// One table provider an installed module supplies (§8.3), as the "new table"
/// screen offers it.
///
/// `config_spec` is the same [`FormField`](sc_types::FormField) declaration a
/// file store's backend and an LLM provider send, so the dialog renders it with
/// no code that knows what a table provider is.
fn table_provider_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("module", TypeSchema::text()),
        StructField::new("provider", TypeSchema::text()),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
    ])
}

/// The overlay fields of a table — everything an admin may set, and nothing the
/// database is the authority on (§9's precedence rule).
fn table_settings_fields() -> Vec<StructField> {
    vec![
        StructField::new("label", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("min_role_read", TypeSchema::int()),
        StructField::new("min_role_write", TypeSchema::int()),
        // The ownership formula source (§7.3); empty means "none". Validated
        // on save: an unknown identifier or a broken Ⱶ-path is a 400 naming
        // it, and nothing is written.
        StructField::new("ownership_formula", TypeSchema::text()),
        // Stored and surfaced in this phase; §6 makes it enforce. Refused on
        // save when the backend cannot do RLS or the formula cannot become a
        // policy.
        StructField::new("rls_enabled", TypeSchema::bool()),
        // Whether offline sync records the table's changes (§14.11). Refused on
        // save without a one-column primary key, or on a database that cannot
        // record changes.
        StructField::new("sync_enabled", TypeSchema::bool()),
        // Whether the table keeps row history (§14.12), off by default. The same
        // refusals as `sync_enabled`; the two share one change trigger.
        StructField::new("history_enabled", TypeSchema::bool()),
        // Who may read the table's history (§14.12): the admin unless a wider
        // role is named, and then each row is checked against its read rule
        // too. Optional, and absent is the admin's.
        StructField::new("history_min_role", TypeSchema::optional(TypeSchema::int())),
        // How many days history keeps a change; absent or 0 keeps it forever.
        // The cleanup job removes older entries, and the files of rows whose
        // delete entry went.
        StructField::new("history_keep_days", TypeSchema::optional(TypeSchema::int())),
    ]
}

/// The body accepted when configuring a table.
///
/// Not optional fields: a settings save states the whole configuration, so an
/// omitted role would have to mean either "leave it" or "reset it" and the wire
/// cannot say which. The admin UI edits a loaded table and sends it back, so it
/// always has every value to hand.
fn table_settings_schema() -> TypeSchema {
    TypeSchema::Struct(table_settings_fields())
}

/// Stored settings for a table that is not in the database (§1.1).
fn orphan_table_settings_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("name", TypeSchema::text())];
    fields.extend(table_settings_fields());
    TypeSchema::Struct(fields)
}
