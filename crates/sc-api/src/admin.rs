//! The admin UI's API, expressed as a fixed [`EndpointSet`] (design §13.1).
//!
//! The admin API is compile-time-known, but rather than a bespoke statically
//! typed router it is built as a set of constant [`Endpoint`] values fed through
//! the **same** [`EndpointSet`] machinery an application uses. This maximises
//! reuse: `ui/admin` consumes a generated typed client (see
//! [`crate::typescript`]) exactly as an application would, and `sc-server`
//! mounts these endpoints the same way it mounts a runtime application's.
//!
//! The [`HandlerRef::Named`] handlers here are resolved by `sc-server` when it
//! mounts the set (the "Server" subphase of Phase 6). This module defines the
//! *contract* — methods, paths, schemas, and auth — that both the server and the
//! generated client are held to.
//!
//! ## The `.mcp()` tags, and why there are so few of them
//!
//! A handful of endpoints below carry an [`Endpoint::mcp`] tag, which offers
//! them to the administration MCP server as tools (§13.6). It is **opt-in and
//! deliberately sparse**: a coding agent pays for every tool in its context on
//! every turn, so projecting all of these would be mechanical and wrong. The
//! thirteen composite tools of `sc_api::mcp` and `sc_app::mcp` plus these seventeen
//! come to thirty, and the number is a design constraint rather than an outcome —
//! `the_tier_two_tags_are_the_ones_that_were_argued_for` is the test that makes
//! adding one a decision somebody has to write down.
//!
//! Three groups are tagged, and each comment above a tag says why that one:
//! what an agent needs to **read** before it can write (`listFieldTypes`,
//! `listAgentTraits`, `listTableProviders`, `listActions`, `listTriggers`), the
//! objects it may be asked to **build** that have no composite tool (the agents,
//! the workflows), and the one thing it must do **after** a schema change
//! (`buildApplication`, since re-projection runs no bundler). Row CRUD, the file
//! store, backup and restore and user management are tier 3 — absent on purpose,
//! not overlooked.

use crate::auth::{credentials_schema, user_row_schema, user_summary_schema};
use crate::endpoint::{
    AuthRequirement, Endpoint, EndpointSet, McpTag, Method, PathSpec, QueryParam,
};
use crate::mcp::{Area, Grant};
use crate::schema::{StructField, TypeSchema, ValueType};

/// Path prefix every admin endpoint is mounted under.
pub const ADMIN_API_PREFIX: &str = "api";

/// The full set of admin API endpoints.
///
/// Grouped as: bootstrap/auth (first-user, login, logout, whoami), catalog
/// (tables + fields), row CRUD, and user management. Each maps to a
/// [`HandlerRef::Named`] the server resolves at mount time.
pub fn admin_endpoints() -> EndpointSet {
    let mut set = EndpointSet::new();

    // --- bootstrap & auth ---------------------------------------------------

    // Whether any user exists yet — drives the create-first-user screen.
    set.register(
        Endpoint::new("authStatus", Method::Get, api().lit("auth/status"))
            .output(TypeSchema::struct_of([
                StructField::new("any_user_exists", TypeSchema::bool()),
                StructField::new("current_user", TypeSchema::optional(user_summary_schema())),
                // Which languages this installation serves (§16.1). Here rather
                // than on the settings payload because this is the call the SPA
                // makes before it renders anything, and three screens need the
                // list: the user menu's locale picker, the user form's language
                // select, and whatever chooses the SPA's own catalogue. It is
                // public for the same reason the rest of this response is — the
                // sign-in page is a page too, and it has a language.
                StructField::new("locales", locales_schema()),
            ]))
            .auth(AuthRequirement::Public),
    );

    // Create the very first (admin) user. Only valid while no user exists.
    set.register(
        Endpoint::new("createFirstUser", Method::Post, api().lit("first-user"))
            .input(credentials_schema())
            .output(user_summary_schema())
            .auth(AuthRequirement::Public),
    );

    set.register(
        Endpoint::new("login", Method::Post, api().lit("login"))
            .input(credentials_schema())
            .output(user_summary_schema())
            .auth(AuthRequirement::Public),
    );

    set.register(
        Endpoint::new("logout", Method::Post, api().lit("logout")).auth(AuthRequirement::LoggedIn),
    );

    // --- catalog: tables & fields ------------------------------------------

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

    // --- metadata tables ------------------------------------------------------
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

    // --- table providers (§8.3) ---------------------------------------------
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

    // --- roles --------------------------------------------------------------
    // A role is a row in `_fd_roles` (§7.1, §9), not a bare integer: it carries
    // a name and, in `attributes`, whatever role-specific settings arrive later.
    // `users.role` is a foreign key onto it, so creating a role is a
    // prerequisite for assigning a user to it — which is why creating and
    // deleting roles has to be reachable from the admin UI and not only from a
    // SQL prompt.

    set.register(
        Endpoint::new("listRoles", Method::Get, api().lit("roles"))
            .output(TypeSchema::array(role_schema()))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("createRole", Method::Post, api().lit("roles"))
            .input(TypeSchema::struct_of([
                StructField::new("role", TypeSchema::int()),
                StructField::new("name", TypeSchema::text()),
                StructField::new("description", TypeSchema::text()),
            ]))
            .output(role_schema())
            .auth(AuthRequirement::admin()),
    );

    // Addressed by the role *number*, not the row id: the number is what
    // `users.role` and every `min_role` holds, so it is the handle an admin
    // already has in front of them.
    set.register(
        Endpoint::new(
            "deleteRole",
            Method::Delete,
            api().lit("roles").param("role", ValueType::Int),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "listFields",
            Method::Get,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("fields"),
        )
        .output(TypeSchema::array(field_schema()))
        .auth(AuthRequirement::admin()),
    );

    // The other half of the fields screen: not what this table points at, but
    // what points at *it*. The catalog already answers this for a drop
    // (`SchemaProjection::referencing_fields`), and it is the same question an
    // admin asks looking at a table — "what breaks if I change this?" — so the
    // rule is not restated here, endpoint or UI. Self-joins are excluded by that
    // rule: a table's key onto itself is already in its own field list.
    set.register(
        Endpoint::new(
            "listInboundKeys",
            Method::Get,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("inbound-keys"),
        )
        .output(TypeSchema::array(TypeSchema::struct_of([
            StructField::new("table", TypeSchema::text()),
            StructField::new("field", TypeSchema::text()),
        ])))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "createField",
            Method::Post,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("fields"),
        )
        .input(create_field_schema())
        .output(field_schema())
        .auth(AuthRequirement::admin()),
    );

    // Overlay-only edits: label, description, rich type, kind parameters,
    // attributes. Renaming or retyping a *column* is a schema change and is out
    // of scope (§3.3), so this endpoint cannot touch `name`, `required`, `unique`
    // or the storage SQL type.
    set.register(
        Endpoint::new(
            "updateField",
            Method::Put,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("fields")
                .param("field", ValueType::Text),
        )
        .input(field_settings_schema())
        .output(field_schema())
        .auth(AuthRequirement::admin()),
    );

    // Drop a field: the column, its data and its overlay row. Refused by name
    // when it is a primary key, a built-in column of `users`/`_fd_roles`, the
    // target of another table's key, or read by a calculated field — each of
    // which the database would otherwise refuse with an error nobody can act on.
    set.register(
        Endpoint::new(
            "deleteField",
            Method::Delete,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("fields")
                .param("field", ValueType::Text),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "dropped",
            TypeSchema::text(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // --- table constraints (§5) ---------------------------------------------
    //
    // A constraint has no `update`, and that is deliberate: what it constrains
    // *is* its identity — a unique constraint over a different pair of fields is
    // a different rule, and a changed formula is a different trigger. So the
    // three verbs are list, create and delete, and "edit" is delete-then-create,
    // which is also exactly what the database would do.
    set.register(
        Endpoint::new(
            "listConstraints",
            Method::Get,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("constraints"),
        )
        .output(TypeSchema::array(constraint_schema()))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "createConstraint",
            Method::Post,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("constraints"),
        )
        .input(create_constraint_schema())
        .output(constraint_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteConstraint",
            Method::Delete,
            api()
                .lit("tables")
                .param("table", ValueType::Text)
                .lit("constraints")
                .param("constraint", ValueType::Text),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "dropped",
            TypeSchema::text(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // The registered field types (basic, rich) and kinds (Key, File) with their
    // attribute specs, so the field editor can render a form for a type it knows
    // nothing about — the same contract `listFrameworks` has (§13.3).
    set.register(
        Endpoint::new("listFieldTypes", Method::Get, api().lit("field-types"))
            .output(TypeSchema::array(field_type_schema()))
            // Tagged (§13.6): the vocabulary `edit_schema` writes fields in.
            // A model that guesses at a rich type's name spends a turn being
            // refused by a list it could have read.
            .mcp(
                "List every field type a table's fields can have — the basic \
                 types, the rich types a module registered, and the `Key` and \
                 `File` kinds — each with the attributes it accepts. This is the \
                 vocabulary `edit_schema` writes a field in.",
            )
            .auth(AuthRequirement::admin()),
    );

    // --- row CRUD -----------------------------------------------------------
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
        .query([QueryParam::new("filter", ValueType::Text).map()])
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

    // --- rows in bulk, as CSV ----------------------------------------------
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

    // --- database connections ----------------------------------------------
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
            "listDatabaseConnections",
            Method::Get,
            api().lit("db-connections"),
        )
        .output(TypeSchema::array(db_connection_schema()))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "createDatabaseConnection",
            Method::Post,
            api().lit("db-connections"),
        )
        .input(db_connection_input_schema())
        .output(db_connection_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateDatabaseConnection",
            Method::Put,
            api().lit("db-connections").param("id", ValueType::Uuid),
        )
        .input(db_connection_input_schema())
        .output(db_connection_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteDatabaseConnection",
            Method::Delete,
            api().lit("db-connections").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Dial what the form currently holds, without saving anything. The
    // configure-scope counterpart of a file store's backend operations, and the
    // thing that makes the form usable: a connection is four boxes that are
    // either all right or silently wrong, and this is how an admin finds out
    // which before committing to a name.
    set.register(
        Endpoint::new(
            "testDatabaseConnection",
            Method::Post,
            api().lit("db-connections").lit("test"),
        )
        .input(db_connection_input_schema())
        .output(TypeSchema::struct_of([
            StructField::new("connected", TypeSchema::bool()),
            StructField::new("error", TypeSchema::optional(TypeSchema::text())),
            StructField::new("tables", TypeSchema::int()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // --- file stores (configuration) ---------------------------------------
    // A file store, like an application, exists only as its stored row (§9,
    // §14.1); these endpoints are the row ⇄ connected-store path the SPA drives.
    // Create/update/delete manage the definition and keep the live registry in
    // step — a renamed store's old handle is disconnected, a deleted store's
    // handle too, so nothing goes on serving a store the admin has removed.
    //
    // Note the addressing split, which is deliberate: these operate on a store's
    // **id**, because the row's identity survives a rename, while the file
    // manager below operates on a store's **name**, because that is what an
    // admin picked and what everything else references.

    // Every *defined* store — not merely every connected one — with whether it
    // is currently connected and, if not, why. A store whose directory has been
    // unmounted must still be listed and editable: editing it is the repair.
    set.register(
        Endpoint::new("listFileStores", Method::Get, api().lit("file-stores"))
            .output(TypeSchema::array(file_store_schema()))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("createFileStore", Method::Post, api().lit("file-stores"))
            .input(file_store_input_schema())
            .output(file_store_schema())
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateFileStore",
            Method::Put,
            api().lit("file-stores").param("id", ValueType::Uuid),
        )
        .input(file_store_input_schema())
        .output(file_store_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteFileStore",
            Method::Delete,
            api().lit("file-stores").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // The registered backends with their settings spec, so the create/edit form
    // can render controls for a backend it knows nothing about — the same move
    // `listFrameworks` makes for frameworks (§13.3).
    set.register(
        Endpoint::new(
            "listFileStoreBackends",
            Method::Get,
            api().lit("file-store-backends"),
        )
        .output(TypeSchema::array(backend_info_schema()))
        .auth(AuthRequirement::admin()),
    );

    // --- backend operations -------------------------------------------------
    // Some backends offer *acts* as well as settings — a git store generates a
    // deploy key, clones, pulls, pushes and commits. Those are declared as data
    // (`Operation`, §6.2) exactly as settings are, and run through these two
    // endpoints, so the admin UI renders a button per declared operation and
    // knows nothing about any particular one. A backend supplied by a plugin
    // gets its buttons the same way a built-in one does; without this the UI
    // would need a branch per backend, and an operation would be something only
    // a built-in backend could have.
    //
    // There are two endpoints because there are two scopes, and the difference
    // is real rather than bookkeeping:

    // **Configure scope** — runs against configuration the admin is still
    // editing, so it is addressed by *backend name* and carries the unsaved
    // config in its body. This is what lets "generate a deploy key" happen
    // before the store exists, which it must: saving a git store clones it, and
    // cloning needs a key the remote already accepts. What comes back is the
    // config with the operation's changes merged in, for the form to adopt.
    set.register(
        Endpoint::new(
            "runBackendOperation",
            Method::Post,
            api()
                .lit("file-store-backends")
                .param("backend", ValueType::Text)
                .lit("operations")
                .param("operation", ValueType::Text),
        )
        .input(TypeSchema::struct_of([
            StructField::new("name", TypeSchema::text()),
            StructField::new("config", TypeSchema::json()),
            StructField::new("input", TypeSchema::json()),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("config", TypeSchema::json()),
            StructField::new("output", TypeSchema::text()),
            StructField::new("data", TypeSchema::optional(TypeSchema::json())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // **Instance scope** — runs against a saved store, addressed by id like the
    // rest of the configuration endpoints. Anything the operation changed in the
    // definition is persisted, and the store is reconnected afterwards, since an
    // operation may be exactly what makes it connectable (a clone).
    //
    // It works from the stored *definition*, not from a connected instance, and
    // that is the point: a git store that has never been cloned has no instance,
    // and cloning it is the operation that would otherwise be unreachable.
    set.register(
        Endpoint::new(
            "runFileStoreOperation",
            Method::Post,
            api()
                .lit("file-stores")
                .param("id", ValueType::Uuid)
                .lit("operations")
                .param("operation", ValueType::Text),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "input",
            TypeSchema::json(),
        )]))
        .output(TypeSchema::struct_of([
            StructField::new("config", TypeSchema::json()),
            StructField::new("output", TypeSchema::text()),
            // Optional, and shaped by whichever backend filled it: `output` is
            // what every client renders, and this is for the one that has to act
            // on the result rather than show it — the IDE's source-control view,
            // which cannot list changed files from a paragraph of prose (§12.1).
            StructField::new("data", TypeSchema::optional(TypeSchema::json())),
            StructField::new("connected", TypeSchema::bool()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // --- LLM providers (configuration) --------------------------------------
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

    // --- LLM models ------------------------------------------------------------
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

    // The model settings a backend declares, so the model form renders
    // controls for a backend it knows nothing about.
    set.register(
        Endpoint::new(
            "listLlmModelSettings",
            Method::Get,
            api()
                .lit("llm-provider-backends")
                .param("backend", ValueType::Text)
                .lit("model-settings"),
        )
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

    // --- modules (Saltcorn v1 JavaScript plugins) ---------------------------
    // A module, like a file store and an LLM provider, exists only as its stored
    // row plus what is on disk, and these endpoints are that row's lifecycle.
    // Two things are different from the others, and both come from the module
    // being *somebody else's code*:
    //
    // - **What it supplies is read from the package, not from the row** — the
    //   actions, their settings and the module's own settings are all in the
    //   listing because they were read at load, never stored, so `npm install`
    //   cannot leave the admin looking at last week's form.
    // - **Everything that went wrong is reported rather than thrown**: a module
    //   that would not load, an action whose name is already taken, a setting of
    //   a type this version does not know. `issues` is that list, and it is why
    //   `listModules` never fails because one module is broken.

    set.register(
        Endpoint::new("listModules", Method::Get, api().lit("modules"))
            .output(TypeSchema::struct_of([
                StructField::new("modules", TypeSchema::array(module_schema())),
                // Where packages are installed on this server, and whether the
                // toolchain that installs them is there — the two things an
                // admin needs before the first install, and neither of which is
                // a property of any module.
                StructField::new("root", TypeSchema::text()),
                StructField::new("npm", TypeSchema::bool()),
                // Present but useless is its own answer: an npm older than the
                // installer's floor cannot resolve the modules root at all, and
                // fails every install with a semver error about a `file:`
                // specifier. Null when npm can install a module; the version
                // that is there and the version that would work when it cannot,
                // because the sentence the tab shows names both.
                StructField::new(
                    "npm_too_old",
                    TypeSchema::optional(TypeSchema::struct_of([
                        StructField::new("version", TypeSchema::text()),
                        StructField::new("minimum", TypeSchema::text()),
                    ])),
                ),
                StructField::new("node", TypeSchema::bool()),
                // The same two questions for the other language (§8): whether
                // there is an interpreter to build the environment with and
                // whether it has pip, asked before an admin types a
                // distribution name. Neither is the same question as whether
                // *this binary* has Python linked in, which is on the
                // Development tab.
                StructField::new("python", TypeSchema::bool()),
                StructField::new("pip", TypeSchema::bool()),
                StructField::new("python_dir", TypeSchema::optional(TypeSchema::text())),
                // The **bundled** catalog: the modules this server ships with
                // and can install from itself, listed whether or not they are
                // installed. It is here rather than on an endpoint of its own
                // because the tab asks one question — "what can this server
                // run, and what is it running" — and two requests to answer it
                // would be two loading states for one screen.
                StructField::new("bundled", TypeSchema::array(bundled_module_schema())),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // **Install**: `npm install` in the modules root, then load. The body is the
    // two things an admin types — which kind of source, and the specifier — and
    // everything else about the module is discovered from the package.
    //
    // A **bundled** module is the same endpoint with nothing typed: `source` is
    // `bundled` and `location` is the catalog id the listing above gave, which
    // is what makes the Install button on a catalog card one click. The
    // language comes from the catalog, and so do the permissions the module is
    // granted — the card printed them beside the button.
    set.register(
        Endpoint::new("installModule", Method::Post, api().lit("modules"))
            .input(TypeSchema::struct_of([
                StructField::new("source", TypeSchema::text()),
                StructField::new("location", TypeSchema::text()),
                // `javascript` when it is not sent, which is what every caller
                // written before there was a second language means (§8) — and
                // for a bundled module, the catalog's answer overrides it.
                StructField::new("language", TypeSchema::optional(TypeSchema::text())),
            ]))
            .output(module_schema())
            .auth(AuthRequirement::admin()),
    );

    // **Configure**: the module's own settings, the object v1's `actions(cfg)`
    // is called with, and its **permissions** — what its worker may reach (§2).
    // A save reloads the module, so the next run of any of its actions uses the
    // new value; a permissions save also *moves* it, onto a worker built with
    // the new set, because a permission set belongs to an isolate.
    set.register(
        Endpoint::new(
            "updateModule",
            Method::Put,
            api().lit("modules").param("id", ValueType::Uuid),
        )
        .input(TypeSchema::struct_of([
            StructField::new("configuration", TypeSchema::optional(TypeSchema::json())),
            // Present only when the admin edited them, so the settings form and
            // the permissions form are two saves rather than one form that has
            // to carry the other's fields to avoid clearing them.
            StructField::new("permissions", TypeSchema::optional(TypeSchema::json())),
        ]))
        .output(module_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteModule",
            Method::Delete,
            api().lit("modules").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // **Reload**: re-read every installed package and rebuild the action set.
    // The developer's button — a module installed from a local checkout is a
    // symlink, so editing the checkout changes the module and nothing else has
    // to happen for this to pick it up.
    //
    // Its own path rather than `modules/reload`, which would sit where an id
    // goes.
    set.register(
        Endpoint::new("reloadModules", Method::Post, api().lit("modules-reload"))
            .output(TypeSchema::struct_of([StructField::new(
                "modules",
                TypeSchema::int(),
            )]))
            .auth(AuthRequirement::admin()),
    );

    // --- agents (configuration) ---------------------------------------------
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

    // --- runs ---------------------------------------------------------------
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

    // --- files (file manager) ----------------------------------------------

    // Browse one directory of a store. A POST (not GET) so the directory — which
    // may contain `/` and would not fit a single path segment — rides in the body.
    set.register(
        Endpoint::new(
            "browseFiles",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("browse"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "dir",
            TypeSchema::text(),
        )]))
        .output(TypeSchema::array(file_listing_entry_schema()))
        .auth(AuthRequirement::admin()),
    );

    // Find entries anywhere under a directory by **name** — the file manager's
    // search box. Not `searchFiles`: that one reads every text file to find a
    // matching *line*, which is the wrong instrument, and much the wrong cost,
    // for "where did I put invoice-2024.pdf". Nothing is read here; the walk
    // needs names, and names are in the listing.
    //
    // The hits are listing entries rather than paths, so a result can be shown
    // in the same table as a directory, with the same columns, without a request
    // per row.
    set.register(
        Endpoint::new(
            "findFiles",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("find"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("query", TypeSchema::text()),
            StructField::new("dir", TypeSchema::optional(TypeSchema::text())),
            StructField::new("max_results", TypeSchema::optional(TypeSchema::int())),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("entries", TypeSchema::array(file_listing_entry_schema())),
            StructField::new("truncated", TypeSchema::bool()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // The files of a kind in a store, by path — what a setting that names a file
    // (`store_files:png,jpg`, an app icon or a keystore) offers as its choices.
    // Dependency and generated directories are skipped, so a project's
    // `node_modules` does not bury the one icon the admin is looking for.
    set.register(
        Endpoint::new(
            "listStoreFiles",
            Method::Get,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("files-by-type"),
        )
        // Comma-separated, without dots: `png,jpg,jpeg`.
        .query([QueryParam::new("extensions", ValueType::Text)])
        .output(TypeSchema::struct_of([
            StructField::new("paths", TypeSchema::array(TypeSchema::text())),
            StructField::new("truncated", TypeSchema::bool()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Search a store's text files, server-side. This is the endpoint the IDE's
    // find-in-files runs on (§12.1): walking the tree through the filesystem
    // provider is one request per directory, and the same walk done where the
    // bytes are is one request in total. `search_files` (§11.3) runs the same
    // search, so what a person finds in the editor is what a model finds.
    set.register(
        Endpoint::new(
            "searchFiles",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("search"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("pattern", TypeSchema::text()),
            StructField::new("regex", TypeSchema::optional(TypeSchema::bool())),
            StructField::new("case_sensitive", TypeSchema::optional(TypeSchema::bool())),
            StructField::new("whole_word", TypeSchema::optional(TypeSchema::bool())),
            StructField::new("glob", TypeSchema::optional(TypeSchema::text())),
            StructField::new("dir", TypeSchema::optional(TypeSchema::text())),
            StructField::new("max_results", TypeSchema::optional(TypeSchema::int())),
        ]))
        .output(file_search_schema())
        .auth(AuthRequirement::admin()),
    );

    // Read one file's bytes (download) — base64 always, plus a UTF-8 `text`
    // shortcut when the contents decode cleanly (for the text editor).
    set.register(
        Endpoint::new(
            "readFile",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("read"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "path",
            TypeSchema::text(),
        )]))
        .output(file_content_schema())
        .auth(AuthRequirement::admin()),
    );

    // Write one file (upload / save an edited text file). The body carries the
    // contents as either base64 (`base64`) or UTF-8 (`text`); exactly one.
    set.register(
        Endpoint::new(
            "writeFile",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("write"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("path", TypeSchema::text()),
            StructField::new("base64", TypeSchema::optional(TypeSchema::text())),
            StructField::new("text", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(file_entry_schema())
        .auth(AuthRequirement::admin()),
    );

    // Create a directory (and any missing parents). Idempotent — asking for a
    // directory that exists is a success, since the caller wanted one there.
    set.register(
        Endpoint::new(
            "makeDirectory",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("mkdir"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "path",
            TypeSchema::text(),
        )]))
        .output(file_entry_schema())
        .auth(AuthRequirement::admin()),
    );

    // Delete a file, or a directory and everything in it. Reports whether
    // anything was there, so the caller need not race an existence check.
    set.register(
        Endpoint::new(
            "deleteFile",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("delete"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "path",
            TypeSchema::text(),
        )]))
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Move or rename within the store. Never overwrites an existing destination.
    set.register(
        Endpoint::new(
            "renameFile",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("rename"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("from", TypeSchema::text()),
            StructField::new("to", TypeSchema::text()),
        ]))
        .output(file_entry_schema())
        .auth(AuthRequirement::admin()),
    );

    // Per-file metadata (design §9): the access rule and the free-form
    // attributes kept beside the bytes rather than in a database row. `min_role`
    // is what the path-cumulative rule is built from, so this is how an admin
    // restricts a folder.
    set.register(
        Endpoint::new(
            "getFileMeta",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("meta"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "path",
            TypeSchema::text(),
        )]))
        .output(file_meta_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "setFileMeta",
            Method::Post,
            api()
                .lit("file-stores")
                .param("store", ValueType::Text)
                .lit("set-meta"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("path", TypeSchema::text()),
            StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
            StructField::new("attributes", TypeSchema::json()),
        ]))
        .output(file_meta_schema())
        .auth(AuthRequirement::admin()),
    );

    // --- applications -------------------------------------------------------
    // An application is created in the admin UI and exists only as its stored
    // row (§13.2); these endpoints are the row ⇄ mounted-app path the SPA drives.
    // Create/update/delete manage the definition; `build` builds and mounts it
    // live (§13.2 "no restart"); the build's outcome — including a bundler's
    // diagnostics on failure — comes back as an Application error (§16).

    set.register(
        Endpoint::new("listApplications", Method::Get, api().lit("applications"))
            .output(TypeSchema::array(application_schema()))
            .mcp(
                McpTag::new(
                    "List the applications: what each is called, the subdomain it \
                 serves on, its framework and project directory, the API \
                 providers it exposes and the tables and triggers behind them. \
                 The `id` here is what `buildApplication` names.",
                )
                .in_area(Area::Applications),
            )
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("createApplication", Method::Post, api().lit("applications"))
            .input(application_input_schema())
            .output(created_application_schema())
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateApplication",
            Method::Put,
            api().lit("applications").param("id", ValueType::Uuid),
        )
        .input(application_input_schema())
        .output(application_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteApplication",
            Method::Delete,
            api().lit("applications").param("id", ValueType::Uuid),
        )
        // `agent` names the builder agent that was deleted with the application
        // (§13.3), when there was one to delete — an application's builder can do
        // nothing once the application is gone, and the admin should be told it
        // went rather than discover it missing.
        .output(TypeSchema::struct_of([
            StructField::new("deleted", TypeSchema::bool()),
            StructField::new("agent", TypeSchema::optional(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Build (and mount) an application. On success the app is serving on its
    // subdomain; the response reports the build log. A failed build leaves the
    // previously mounted version up and comes back as an Application error whose
    // message carries the bundler's own diagnostics (§16).
    set.register(
        Endpoint::new(
            "buildApplication",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("build"),
        )
        .output(build_result_schema())
        // Tagged (§13.6) because of the one gap re-projection leaves: a schema
        // change rewrites an application's generated client but runs no bundler,
        // so an agent that changed a schema needs a way to rebuild what it
        // changed. `edit_schema`'s result names the applications that want one.
        .mcp(
            McpTag::new(
                "Build an application and mount it live: regenerates its typed \
             client from the current schema, runs its framework's build, and \
             serves the result on its subdomain with no restart. Run this after \
             a schema change that `edit_schema` reported as affecting an \
             application with a build. A failed build is a result rather than a \
                 refusal: `built` is false, `log` is what the build tools said \
                 and `diagnostics` lists the file, line and message of each \
                 error to fix. The previously built version keeps serving until \
                 one succeeds.",
            )
            .in_area(Area::Applications)
            // Building writes an application's generated client and replaces
            // what its subdomain serves. That is a change to what is there,
            // which is `allow_edit` — not a create, whatever the method says.
            .needs(Grant::Edit)
            // And a build that did not compile is news about the application,
            // not a refusal of the call: the tool result carries the tools'
            // output with its diagnostics parsed out, which is what an agent
            // that just changed a schema has to read to fix what it broke.
            .is_a_build(),
        )
        .auth(AuthRequirement::admin()),
    );

    // **Deep clean**: delete an application's installed dependencies (a `react`
    // app's `node_modules`) and build it again, which installs them from
    // scratch. For the tree a plain build cannot fix — an interrupted install,
    // a corrupted cache, dependencies changed by hand. The result is a build's,
    // because that is what it ends in; a framework that installs nothing is
    // refused. Not an MCP tool: it is slow, and it is the admin's remedy for a
    // broken machine rather than a step in building an application.
    set.register(
        Endpoint::new(
            "deepCleanApplication",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("deep-clean"),
        )
        .output(build_result_schema())
        .auth(AuthRequirement::admin()),
    );

    // Build one of the targets an application's framework offers beside its web
    // bundle — an Android APK. A native build is minutes long, so this **starts**
    // it and answers at once with the job, `running`; the UI then asks
    // `getApplicationTargetBuild` until it is done. A second start while one is
    // running answers that one rather than starting another in the same project.
    // Not a mount: the result is a file left in the application's store. Not
    // offered over MCP: nothing an agent changing a schema needs to do.
    set.register(
        Endpoint::new(
            "buildApplicationTarget",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("targets")
                .param("target", ValueType::Text)
                .lit("build"),
        )
        .output(target_build_schema())
        .auth(AuthRequirement::admin()),
    );

    // Run an operation a target declares — what a button under the target's
    // settings does, such as generating a signing keystore. The module makes the
    // files and settings; the server writes the files into the application's
    // store (never over an existing one) and saves the settings on it. The body
    // is the form's current framework settings, unsaved edits included, so the
    // module sees what the admin sees.
    set.register(
        Endpoint::new(
            "runApplicationTargetOperation",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("targets")
                .param("target", ValueType::Text)
                .lit("operations")
                .param("operation", ValueType::Text),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "config",
            TypeSchema::json(),
        )]))
        .output(TypeSchema::struct_of([
            StructField::new("message", TypeSchema::text()),
            StructField::new("store", TypeSchema::text()),
            StructField::new("files", TypeSchema::array(TypeSchema::text())),
            // Whether the store is a git repository, whose next commit would
            // carry the files.
            StructField::new("git_repo", TypeSchema::bool()),
            // The settings it set, as now stored, secrets masked.
            StructField::new("settings", TypeSchema::json()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // The latest build of a target, running or finished — what the UI polls, and
    // what it asks after a reload to find a build still running. A 404 when this
    // process has not built that target since it started.
    set.register(
        Endpoint::new(
            "getApplicationTargetBuild",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("targets")
                .param("target", ValueType::Text)
                .lit("build"),
        )
        .output(target_build_schema())
        .auth(AuthRequirement::admin()),
    );

    // Rewrite an application's **generated** code from its current definition —
    // `src/feldspar/**`: the typed client, the hooks, the schema and the README
    // (§13.3). No bundler runs; this is the "if the API definition changes, the
    // client code must be updated automatically" path (decision 10) with a
    // button on it, for the times an admin wants it *now* rather than at the
    // next change.
    //
    // A project directory that is **empty** is scaffolded instead — an app whose
    // store was unreachable when it was created, or whose tree somebody deleted,
    // has nothing to regenerate, and a `src/feldspar/` with no project around it
    // could not build. Which of the two happened is in the response, because
    // writing a whole project is not the same news as rewriting four files.
    set.register(
        Endpoint::new(
            "updateApplicationClient",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("client"),
        )
        .output(TypeSchema::struct_of([
            StructField::new("scaffolded", TypeSchema::bool()),
            StructField::new("files", TypeSchema::array(TypeSchema::text())),
            StructField::new("log", TypeSchema::text()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Run one GraphQL operation against a **mounted** application's own GraphQL
    // provider — what the admin UI's explorer (§13.4) is built on.
    //
    // It is an admin endpoint rather than a browser request to the app's mount
    // because the admin SPA is served under `connect-src 'self'`: a `fetch` from
    // the base domain to `blog.example.com/graphql` is a cross-origin request
    // the page's own policy forbids, and relaxing that policy to let one screen
    // talk to every subdomain is a poor trade for a debugging tool.
    //
    // **The operation runs as the signed-in admin**, through the very same
    // `ApiProvider::handle` a request to the app's mount reaches — same schema,
    // same limits, same authorization at resolve time. The explorer therefore
    // has exactly the authority of the person using it, which is the property
    // that makes it a debugging tool rather than a back door; the screen says so
    // in as many words.
    //
    // The output is `json`: a GraphQL response is `{data, errors, extensions}`
    // whose `data` is the shape the *caller's document* asked for, which no
    // `TypeSchema` can describe ahead of time. That is the same reason the
    // provider's own `graphqlQuery` endpoint declares it.
    set.register(
        Endpoint::new(
            "runApplicationGraphql",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("graphql"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("query", TypeSchema::text()),
            StructField::new("variables", TypeSchema::optional(TypeSchema::json())),
            StructField::new("operationName", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(TypeSchema::json())
        .auth(AuthRequirement::admin()),
    );

    // --- Saltcorn UI views and pages -----------------------------------------
    // A Saltcorn UI application's source is rows rather than a file store
    // (TODO "Saltcorn UI" §1): its views and pages, each addressed by name
    // within the application. A save is the whole deployment — the mounted app
    // reads the reloaded set on its next request — so there is no build to
    // follow one with.
    //
    // `saveView`/`savePage` save the record the path names, creating it when
    // there is none; a body naming something else renames it. What a view may
    // name — the pattern, the table, the role, the actions — is refused on save
    // with a sentence naming it, and the configuration is replayed through the
    // pattern's own configuration steps, so a value a step's form would not
    // accept is refused naming the step and the field (Phase 10).
    set.register(
        Endpoint::new(
            "listViews",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("views"),
        )
        .output(TypeSchema::array(view_schema()))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "getView",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("views")
                .param("name", ValueType::Text),
        )
        .output(view_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "saveView",
            Method::Put,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("views")
                .param("name", ValueType::Text),
        )
        .input(view_input_schema())
        .output(view_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteView",
            Method::Delete,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("views")
                .param("name", ValueType::Text),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // A new view (TODO "Saltcorn UI" 10.2): its name, pattern, table and role,
    // configured as the pattern's `initial_config` starts one — a List over its
    // table's columns. Refused as a save is, and refused if the name is taken,
    // where a save would overwrite.
    set.register(
        Endpoint::new(
            "createView",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("views"),
        )
        .input(create_view_input_schema())
        .output(view_schema())
        .auth(AuthRequirement::admin()),
    );

    // One step of a view's configuration wizard (10.1): the pattern's
    // `configuration_workflow` step `step`, over the table and the context the
    // earlier steps gathered, as the form fields the admin UI renders. A call
    // per step rather than a form per pattern, because a step's form does not
    // exist without its context: List's *Default state* lists the table's
    // fields, and a ListShowList's views are the views over its table.
    set.register(
        Endpoint::new(
            "viewConfigStep",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("view-config-step"),
        )
        .input(view_config_step_input_schema())
        .output(view_config_step_schema())
        .auth(AuthRequirement::admin()),
    );

    // What refers to a view by name (10.4) — the views that embed it or link
    // to it, by their patterns' own `connectedObjects`, and the pages that show
    // it — so a rename can say before it happens what it will leave pointing at
    // a name that no longer exists. Nothing is rewritten.
    set.register(
        Endpoint::new(
            "viewReferences",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("views")
                .param("name", ValueType::Text)
                .lit("references"),
        )
        .output(view_references_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "listPages",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("pages"),
        )
        .output(TypeSchema::array(page_schema()))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "getPage",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("pages")
                .param("name", ValueType::Text),
        )
        .output(page_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "savePage",
            Method::Put,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("pages")
                .param("name", ValueType::Text),
        )
        .input(page_input_schema())
        .output(page_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deletePage",
            Method::Delete,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("pages")
                .param("name", ValueType::Text),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // --- The builder (TODO "The builder" §6, §10) --------------------------------
    // What `ui/builder` calls, and nothing it does not: the two layout saves,
    // what names a page, the application's library, and the calls the canvas
    // makes for its previews and lookups — v1's server routes, ported into the
    // worker and made as the admin. Every one refuses an application that is not
    // a Saltcorn UI application. The builder's options are not an endpoint: the
    // builder route renders them into its document.

    // A view's layout from one of its pattern's builder steps (§6): merged into
    // the configuration where the step keeps it, checked as `saveView` checks —
    // the store, the replay of the other steps, the actions — and saved with the
    // library edits it carries in one transaction, moving the generation once.
    set.register(
        Endpoint::new(
            "saveViewLayout",
            Method::Put,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("views")
                .param("name", ValueType::Text)
                .lit("layout"),
        )
        .input(view_layout_input_schema())
        .output(view_schema())
        .auth(AuthRequirement::admin()),
    );

    // A page's layout (§6): it replaces the page's, is checked as a page is —
    // its actions are v1's page actions or the application's triggers, and the
    // views it shows are the application's — and is saved with its library
    // edits in one transaction.
    set.register(
        Endpoint::new(
            "savePageLayout",
            Method::Put,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("pages")
                .param("name", ValueType::Text)
                .lit("layout"),
        )
        .input(page_layout_input_schema())
        .output(page_schema())
        .auth(AuthRequirement::admin()),
    );

    // What names a page (§10): the menu entries opening it, the roles whose home
    // page it is, and the views, pages and library items whose layouts show or
    // link to it — so a rename or a delete can say beforehand what it leaves
    // pointing at a name that no longer exists. `places` is the other direction:
    // the library items its own layout places. Nothing is rewritten.
    set.register(
        Endpoint::new(
            "pageReferences",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("pages")
                .param("name", ValueType::Text)
                .lit("references"),
        )
        .output(page_references_schema())
        .auth(AuthRequirement::admin()),
    );

    // The application's library (§8): its items, each with the views, pages
    // and other items that place it.
    set.register(
        Endpoint::new(
            "listLibrary",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("library"),
        )
        .output(TypeSchema::array(listed_library_item_schema()))
        .auth(AuthRequirement::admin()),
    );

    // One item, read fresh rather than from the set the builder was opened
    // with: v1's `/library/content/:id`, which a placed instance starts from so
    // it shows the latest layout.
    set.register(
        Endpoint::new(
            "getLibraryItem",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("library")
                .param("item", ValueType::Uuid),
        )
        .output(library_item_schema())
        .auth(AuthRequirement::admin()),
    );

    // v1's `/library/savefrombuilder`: a new item from what the builder
    // selected. A name the application's library already has is refused naming
    // it, as is a layout placing an item the application does not have.
    set.register(
        Endpoint::new(
            "createLibraryItem",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("library"),
        )
        .input(create_library_item_input_schema())
        .output(library_item_schema())
        .auth(AuthRequirement::admin()),
    );

    // Rename an item, or change its icon or description. Its layout is the
    // builder's, and changes through a layout save or `saveLibraryUpdates`.
    set.register(
        Endpoint::new(
            "saveLibraryItem",
            Method::Put,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("library")
                .param("item", ValueType::Uuid),
        )
        .input(save_library_item_input_schema())
        .output(library_item_schema())
        .auth(AuthRequirement::admin()),
    );

    // v1's `/library/save-updates`: edits made inside placed items, apart from
    // any view or page, all or none.
    set.register(
        Endpoint::new(
            "saveLibraryUpdates",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("library")
                .lit("updates"),
        )
        .input(library_updates_input_schema())
        .output(TypeSchema::struct_of([StructField::new(
            "updated",
            TypeSchema::int(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Delete an item. One that something places is refused with `409` and the
    // `references` beside the error, unless `confirm` is true; what placed it
    // then renders blank, which is v1's `resolveSegment` behaviour. The answer
    // names what did.
    set.register(
        Endpoint::new(
            "deleteLibraryItem",
            Method::Delete,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("library")
                .param("item", ValueType::Uuid),
        )
        .query([QueryParam::new("confirm", ValueType::Bool)])
        .output(TypeSchema::struct_of([
            StructField::new("deleted", TypeSchema::bool()),
            StructField::new("references", library_references_schema()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // --- Translations (§16.1, task 4.4) -----------------------------------------
    // An application's own strings — type B, the admin's, written while they
    // built the application. One read and three writes, because the screen is
    // one table: what the source says, what each locale has, what nobody
    // wrapped, and what is left over.

    // The whole screen in one call. Untyped `messages` and `locales` because a
    // catalogue is a *map* whose keys are English sentences: there is no struct
    // to declare, and an endpoint schema that pretended otherwise would be
    // describing a shape that does not exist.
    set.register(
        Endpoint::new(
            "getTranslations",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("translations"),
        )
        .output(TypeSchema::json())
        .auth(AuthRequirement::admin()),
    );

    // Which locales the application serves, and which one it falls back to.
    // Turning one off does **not** delete its catalogue.
    set.register(
        Endpoint::new(
            "setApplicationLocales",
            Method::Put,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("locales"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("locales", TypeSchema::array(TypeSchema::text())),
            StructField::new("default_locale", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(TypeSchema::json())
        .auth(AuthRequirement::admin()),
    );

    // Save one locale's catalogue, whole. Refused — naming the key — for a
    // translation whose placeholders or plural categories differ from its
    // key's, which is the same check the LLM's answers get: the admin typing
    // one by hand is owed the same guarantee.
    set.register(
        Endpoint::new(
            "saveTranslations",
            Method::Put,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("translations")
                .param("locale", ValueType::Text),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "messages",
            TypeSchema::json(),
        )]))
        .output(TypeSchema::json())
        .auth(AuthRequirement::admin()),
    );

    // **Translate missing**: fill everything this locale has not got through
    // the configured LLM, and save it. The answer names every message the
    // placeholder check rejected (D9).
    set.register(
        Endpoint::new(
            "translateMissing",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("translations")
                .param("locale", ValueType::Text)
                .lit("fill"),
        )
        .output(TypeSchema::json())
        .auth(AuthRequirement::admin()),
    );

    // v1's `/field/preview/:table/:field/:fieldview`: a fieldview rendered over
    // the first row the admin can read, as HTML for the canvas.
    set.register(
        Endpoint::new(
            "builderFieldPreview",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("builder")
                .lit("field-preview"),
        )
        .input(builder_field_preview_input_schema())
        .output(builder_html_schema())
        .auth(AuthRequirement::admin()),
    );

    // v1's `/field/fieldviewcfgform/:table?accept=json`: a fieldview's
    // configuration fields, as v1's form JSON — which is the builder's to render,
    // so it is `json` here.
    set.register(
        Endpoint::new(
            "builderFieldviewConfigForm",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("builder")
                .lit("fieldview-config"),
        )
        .input(builder_fieldview_config_input_schema())
        .output(TypeSchema::json())
        .auth(AuthRequirement::admin()),
    );

    // v1's `/view/:name/preview`: an embedded view rendered for the canvas with
    // the state given.
    set.register(
        Endpoint::new(
            "builderViewPreview",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("builder")
                .lit("view-preview"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("view", TypeSchema::text()),
            StructField::new("state", TypeSchema::optional(TypeSchema::json())),
        ]))
        .output(builder_html_schema())
        .auth(AuthRequirement::admin()),
    );

    // v1's `/page/:name/preview`: an embedded page rendered for the canvas.
    set.register(
        Endpoint::new(
            "builderPagePreview",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("builder")
                .lit("page-preview"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "page",
            TypeSchema::text(),
        )]))
        .output(builder_html_schema())
        .auth(AuthRequirement::admin()),
    );

    // v1's `/api/:table/distinct/:field`, which the builder's *Tabs* element
    // asks for a field's values: v1's `{ success: [...] }`, as the admin, for a
    // table in the application's subset only (§3). v1's public row API behind it
    // is not a route on the subdomain, and this does not add one.
    set.register(
        Endpoint::new(
            "builderDistinctValues",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("builder")
                .lit("distinct")
                .param("table", ValueType::Text)
                .param("field", ValueType::Text),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "success",
            TypeSchema::json(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // The view patterns a view may be saved with: the registry save checks
    // against, described by the view runtime where one is running. Server-wide
    // rather than per application, because a pattern is the server's.
    set.register(
        Endpoint::new("listViewPatterns", Method::Get, api().lit("view-patterns"))
            .output(TypeSchema::array(view_pattern_schema()))
            .auth(AuthRequirement::admin()),
    );

    // Whether this server has the builder bundle (TODO "The builder" §2, §9), so
    // the admin UI offers **Open in builder** only where it opens something. A
    // binary built without `ui/builder` answers `false`, and the admin screens
    // keep a layout as read-only JSON with a sentence saying why.
    set.register(
        Endpoint::new("builderStatus", Method::Get, api().lit("builder"))
            .output(TypeSchema::struct_of([StructField::new(
                "available",
                TypeSchema::bool(),
            )]))
            .auth(AuthRequirement::admin()),
    );

    // --- frameworks ---------------------------------------------------------
    // The registered frameworks with their settings spec, so the create/edit
    // form can render controls for a framework it knows nothing about (§13.3).
    set.register(
        Endpoint::new("listFrameworks", Method::Get, api().lit("frameworks"))
            .output(TypeSchema::array(framework_info_schema()))
            .auth(AuthRequirement::admin()),
    );

    // --- API providers ------------------------------------------------------
    // The registered API providers, so the application form offers them as a
    // **list** rather than a free-text box (§13.4). A provider name is the one
    // field of an application whose typo is not caught until the app is mounted,
    // where it becomes "unknown API provider" on a save that appeared to work.
    // The same move `listFrameworks` makes, for the same reason.
    set.register(
        Endpoint::new("listApiProviders", Method::Get, api().lit("api-providers"))
            .output(TypeSchema::array(api_provider_info_schema()))
            .auth(AuthRequirement::admin()),
    );

    // Prepare one custom SQL query and report the columns the **database** says
    // it returns (§13.4, decision 5), without storing anything.
    //
    // Saving already does this — a query that will not prepare cannot be saved —
    // so why a second route? Because the editor needs the answer *before* the
    // save: the columns are what the admin's generated client method will
    // return, and finding out by saving the whole application means finding out
    // about a typo in one `SELECT` by having every other edit on the screen
    // refused with it. It is the same call on the same catalog; only the moment
    // differs.
    set.register(
        Endpoint::new(
            "describeCustomQuery",
            Method::Post,
            api().lit("custom-queries").lit("describe"),
        )
        .input(custom_query_input_schema())
        .output(TypeSchema::struct_of([StructField::new(
            "columns",
            TypeSchema::array(query_column_schema()),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // --- users --------------------------------------------------------------

    // The users screen manages accounts the same way every other screen manages
    // its objects — a list, a form, and the operations that are not edits.
    //
    // Four of these are *not* row edits and that is why they are endpoints of
    // their own rather than fields of `updateUser`: disabling an account,
    // dropping its sessions, becoming it, and resetting its password each do
    // something a column write cannot (they touch sessions, or they hand back a
    // secret that exists only in that response). Spelling them as booleans in an
    // update body would hide that.

    set.register(
        Endpoint::new("listUsers", Method::Get, api().lit("users"))
            .output(TypeSchema::array(user_row_schema()))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("createUser", Method::Post, api().lit("users"))
            .input(user_input_schema())
            // The row, plus the password if one was generated because the admin
            // left it blank — the only moment it is readable (§7.1).
            .output(TypeSchema::struct_of([
                StructField::new("user", user_row_schema()),
                StructField::new(
                    "generated_password",
                    TypeSchema::optional(TypeSchema::text()),
                ),
            ]))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateUser",
            Method::Put,
            api().lit("users").param("id", ValueType::Uuid),
        )
        .input(user_input_schema())
        .output(user_row_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteUser",
            Method::Delete,
            api().lit("users").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "setUserDisabled",
            Method::Post,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("disabled"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "disabled",
            TypeSchema::bool(),
        )]))
        .output(user_row_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "forceLogoutUser",
            Method::Post,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("force-logout"),
        )
        // `ok`, not a count of sessions ended: the sessions are dropped by the
        // transport (which owns the store) after the handler has returned, so a
        // number here would be one the handler had to guess.
        .output(TypeSchema::struct_of([StructField::new(
            "ok",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Swap the caller's own session for one belonging to this user, with no
    // password: an admin who can reset that password can already sign in as them
    // (§7.1), so this adds convenience rather than authority — and it costs the
    // admin their admin session, which is the honest price of the swap.
    set.register(
        Endpoint::new(
            "becomeUser",
            Method::Post,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("become"),
        )
        .output(user_summary_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "setRandomPassword",
            Method::Post,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("random-password"),
        )
        .output(TypeSchema::struct_of([
            StructField::new("email", TypeSchema::text()),
            StructField::new("password", TypeSchema::text()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // --- API tokens (§13.6) --------------------------------------------------
    //
    // The credential the administration MCP server authenticates with: a bearer
    // token that **names a user** and runs with exactly their authority, so
    // there is no second authorization model to keep in step with this one.
    //
    // Three endpoints, and the shape of them is decided by two rules:
    //
    // - **None of them is tagged for MCP.** A token that can mint tokens is a
    //   token that cannot be revoked: the agent holding one could replace it the
    //   moment an admin took it away, and the credential would outlive the
    //   decision to end it. Minting is a thing a person does at a screen.
    // - **The plaintext appears in exactly one response**, `createApiToken`'s.
    //   Nothing reads it back afterwards because nothing can — the table holds a
    //   hash — and `listApiTokens` has no field for it or for the hash.
    //
    // A mint is always for the **calling admin**. Not a limitation of the
    // storage — the row names any user — but of this API: minting a credential
    // that runs as somebody else is handing out their authority without their
    // knowledge, and the person who wants a token is standing at the screen.

    set.register(
        Endpoint::new("listApiTokens", Method::Get, api().lit("api-tokens"))
            .output(TypeSchema::array(api_token_schema()))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("createApiToken", Method::Post, api().lit("api-tokens"))
            .input(TypeSchema::struct_of([
                StructField::new("label", TypeSchema::text()),
                // The six flags of §13.6, each optional so a client that sends
                // none gets the same safe defaults an unconfigured copilot has:
                // it can build, it cannot drop, it cannot widen access.
                StructField::new("grants", TypeSchema::optional(api_token_grants_schema())),
                // Days, not a timestamp: an admin decides how long a laptop
                // keeps a credential, not the instant it stops. Absent means a
                // token that does not lapse, which the screen has to say plainly.
                StructField::new("expires_in_days", TypeSchema::optional(TypeSchema::int())),
            ]))
            .output(TypeSchema::struct_of([
                StructField::new("token", api_token_schema()),
                // Shown once. The response is the only place this value ever
                // exists outside the client that receives it.
                StructField::new("secret", TypeSchema::text()),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // Revocation rather than deletion, and a `POST` rather than a `DELETE`: the
    // row stays, marked, because a revocation is a thing that happened and the
    // list is where an admin sees that it did.
    set.register(
        Endpoint::new(
            "revokeApiToken",
            Method::Post,
            api()
                .lit("api-tokens")
                .param("id", ValueType::Uuid)
                .lit("revoke"),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "revoked",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // --- triggers -----------------------------------------------------------
    // A trigger is one event bound to one configured action (§10.2), stored in
    // `_fd_triggers`. These endpoints are the row ⇄ live-set path the SPA
    // drives: every save is validated and the live set is reloaded, so what the
    // list shows is what will fire.

    set.register(
        Endpoint::new("listTriggers", Method::Get, api().lit("triggers"))
            .output(TypeSchema::array(trigger_schema()))
            // `describe_triggers` is the composite tool over the same rows and
            // says more about each; this is tagged as well because it carries
            // the trigger **ids** the workflow tools address a trigger by, and
            // that composite deliberately speaks in names.
            .mcp(
                McpTag::new(
                    "List the triggers with their ids: the event each fires on, the \
                 table where there is one, the action or workflow it runs, and \
                 whether it is enabled. The id is what the workflow tools \
                 address a workflow by, since a workflow *is* a trigger's \
                 body.",
                )
                .in_area(Area::Triggers),
            )
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("createTrigger", Method::Post, api().lit("triggers"))
            .input(trigger_input_schema())
            .output(trigger_schema())
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateTrigger",
            Method::Put,
            api().lit("triggers").param("id", ValueType::Uuid),
        )
        .input(trigger_input_schema())
        .output(trigger_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteTrigger",
            Method::Delete,
            api().lit("triggers").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Run one trigger now — the admin's "test this now". The posted body is the
    // event's payload and the action's result comes back; an action that fails
    // comes back as an **error**, not a 200 carrying a failure nobody reads.
    set.register(
        Endpoint::new(
            "runTrigger",
            Method::Post,
            api()
                .lit("triggers")
                .param("id", ValueType::Uuid)
                .lit("run"),
        )
        .input(TypeSchema::json())
        .output(TypeSchema::struct_of([StructField::new(
            "result",
            TypeSchema::json(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Run one trigger as a **test**, from the admin's list. Everything about it
    // that differs from `runTrigger` is there because a person is watching:
    //
    //   - a failing action is a **200 carrying the failure**, not an error
    //     status. A test run that failed did its job; the message is the
    //     answer, and it arrives beside everything else the run produced
    //     rather than in place of it.
    //   - what the body **printed** comes back with it. An admin debugging a
    //     `run_js_code` adds a `console.log` and expects to see it — in v1 they
    //     read it off the server's terminal, which a hosted deployment does not
    //     have.
    //   - a trigger on `insert`/`update`/`delete` is run against a **row picked
    //     at random** from its table, because a table trigger has no occurrence
    //     of its own when a person presses a button, and a body that reads
    //     `row.title` against nothing fails for a reason about the test rather
    //     than about the trigger. Which row it was comes back, so a surprising
    //     result is traceable to the row that produced it.
    set.register(
        Endpoint::new(
            "testRunTrigger",
            Method::Post,
            api()
                .lit("triggers")
                .param("id", ValueType::Uuid)
                .lit("test-run"),
        )
        .input(TypeSchema::json())
        .output(TypeSchema::struct_of([
            StructField::new("ok", TypeSchema::bool()),
            StructField::new("result", TypeSchema::json()),
            // The failure, when there was one: the action's own message, which
            // is the whole point of a test run.
            StructField::new("error", TypeSchema::optional(TypeSchema::text())),
            StructField::new(
                "console",
                TypeSchema::array(TypeSchema::struct_of([
                    StructField::new("level", TypeSchema::text()),
                    StructField::new("text", TypeSchema::text()),
                ])),
            ),
            // The row this was run against, for a table event, and `null` for
            // every other kind — including a table trigger whose table is empty,
            // which the screen says rather than pretending a row was chosen.
            StructField::new("row", TypeSchema::optional(TypeSchema::json())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // --- actions ------------------------------------------------------------
    // The registered actions with the settings each declares, so the trigger
    // form renders a configuration form for an action it knows nothing about
    // (§13.3) — the same move the framework and file-store pickers make.
    //
    // `?table=` is optional and names the table the trigger being edited fires
    // on, because an action's declaration may **depend on it**: `send_email`
    // offers one attachment checkbox per File field of that table
    // (`Action::config_spec_for`). Without it the answer is the table-independent
    // spec, which is what every other action has and what a non-table trigger
    // gets. The screen still knows nothing about any particular setting — it asks
    // for the declaration for the table it is showing and renders what comes
    // back.
    set.register(
        Endpoint::new("listActions", Method::Get, api().lit("actions"))
            .query([QueryParam::new("table", ValueType::Text)])
            .output(TypeSchema::array(action_info_schema()))
            // The list; `describe_action` is the one action's settings in full,
            // which is progressive disclosure and stays the way to configure
            // one. This answers "what is there?" in one call rather than N.
            .mcp(
                McpTag::new(
                    "List the actions a trigger can run, each with a summary of what \
                 it does. Pass `table` to see the actions as they are declared \
                 for a trigger on that table, since an action's settings may \
                 depend on it. Then call `describe_action` for the one you want, \
                 which is where the full settings are.",
                )
                .in_area(Area::Triggers),
            )
            .auth(AuthRequirement::admin()),
    );

    // --- workflows ----------------------------------------------------------
    // A workflow is a **trigger body** (§10.3, decision 1), so it is addressed by
    // the trigger's id and has no create or delete of its own: creating the
    // trigger creates version 1, and deleting the trigger takes its versions with
    // it. What is left is what a workflow has that an action body does not — a
    // program, and a history of the programs it used to be.
    //
    // **Versions are rows and the table is append-only** (decision 2). So there
    // is no `updateWorkflow`: `saveWorkflow` mints the next version, and
    // `revertWorkflow` mints a new version whose steps are an old one's, because
    // rewriting history is exactly what append-only says no to — and because a
    // run suspended on version 1 has to still be able to load version 1
    // tomorrow.

    set.register(
        Endpoint::new(
            "getWorkflow",
            Method::Get,
            api().lit("workflows").param("id", ValueType::Uuid),
        )
        // Which version to read, defaulting to the current one. A run is
        // **pinned** to the version it started on (decision 2), so the screen
        // that draws a run on the canvas it ran on has to be able to ask for
        // that version rather than for today's — otherwise "the path taken,
        // highlighted on the same graph the admin drew" would be the path of one
        // program drawn on the picture of another.
        .query([QueryParam::new("version", ValueType::Int)])
        .output(workflow_schema())
        .mcp(
            McpTag::new(
                "Read a workflow — the program a trigger's body runs. Addressed by \
             the **trigger's** id (from `listTriggers`), because a workflow is a \
             trigger body rather than an object of its own. Pass `version` to \
                 read an older one; the default is the current version.",
            )
            .in_area(Area::Triggers),
        )
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "saveWorkflow",
            Method::Post,
            api().lit("workflows").param("id", ValueType::Uuid),
        )
        .input(TypeSchema::struct_of([
            // The whole program — the steps, the start, the workflow-level error
            // policy, the trace flag and the step budget — in the **stored**
            // shape, which is also the editor's. Passed through as JSON rather
            // than described field by field for the reason a run's `context` is:
            // the document's shape belongs to the engine, and a second
            // declaration of it here would be a second spelling to keep in step
            // (decision: the stored JSON is the API shape, and there is no
            // third).
            StructField::new("workflow", TypeSchema::json()),
            // The commit message. A version history without one is a list of
            // numbers.
            StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(workflow_schema())
        .mcp(
            McpTag::new(
                "Save a workflow's steps as a **new version** — the table is \
             append-only, so nothing is overwritten and a run already suspended \
             on an earlier version still loads that one. Send the whole program \
             in the shape `getWorkflow` returns it, with a description saying \
                 what changed. A `run_js_code` step's code is JavaScript against \
                 this server's own `db` API — call `describe_code_api` before \
                 writing one.",
            )
            .in_area(Area::Triggers)
            // A workflow is an existing trigger's body — version 1 is created
            // with the trigger — so writing one is an edit rather than a create.
            .needs(Grant::Edit),
        )
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "revertWorkflow",
            Method::Post,
            api()
                .lit("workflows")
                .param("id", ValueType::Uuid)
                .lit("revert"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("version", TypeSchema::int()),
            StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(workflow_schema())
        .mcp(
            McpTag::new(
                "Go back to an earlier version of a workflow by minting a new \
             version whose steps are that one's. History is not rewritten, so \
                 the version you reverted from is still readable.",
            )
            .in_area(Area::Triggers)
            // Reverting mints a version rather than removing one, so it is an
            // edit and not a drop: nothing stops being readable.
            .needs(Grant::Edit),
        )
        .auth(AuthRequirement::admin()),
    );

    // The runs of one workflow, newest first. Filterable by state and paged,
    // because a workflow that fires on every insert has as many runs as the table
    // has rows and the screen wants the ones that are stuck.
    set.register(
        Endpoint::new(
            "listWorkflowRuns",
            Method::Get,
            api()
                .lit("workflows")
                .param("id", ValueType::Uuid)
                .lit("runs"),
        )
        .query([
            QueryParam::new("state", ValueType::Text),
            QueryParam::new("limit", ValueType::Int),
            QueryParam::new("offset", ValueType::Int),
        ])
        .output(TypeSchema::array(run_summary_schema()))
        .mcp(
            McpTag::new(
                "List a workflow's runs, newest first, addressed by the trigger's \
             id. Filter by `state` — `waiting` is the one that wants attention — \
             and page with `limit` and `offset`, because a workflow that fires \
                 on every insert has as many runs as the table has rows. \
                 `getRun` reads one in full.",
            )
            .in_area(Area::Triggers),
        )
        .auth(AuthRequirement::admin()),
    );

    // --- the three things an admin does to a run ----------------------------
    // On `runs/{id}` rather than under a workflow, because a run is addressed by
    // its own id everywhere else (`getRun`, `deleteRun`) and knowing which
    // workflow it is of is the server's job, not the caller's.

    set.register(
        Endpoint::new(
            "resumeRun",
            Method::Post,
            api().lit("runs").param("id", ValueType::Uuid).lit("resume"),
        )
        // The answers to the step's own form, keyed by its declared field names —
        // checked against that declaration, which is the one the person was shown
        // rather than the workflow's current text.
        .input(TypeSchema::json())
        .output(run_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "cancelRun",
            Method::Post,
            api().lit("runs").param("id", ValueType::Uuid).lit("cancel"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "reason",
            TypeSchema::optional(TypeSchema::text()),
        )]))
        .output(run_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "retryRun",
            Method::Post,
            api().lit("runs").param("id", ValueType::Uuid).lit("retry"),
        )
        .output(run_schema())
        .auth(AuthRequirement::admin()),
    );

    // --- predictive models --------------------------------------------------
    // Five nouns and three screens (TODO "Predictive models"): a **model
    // provider** is code that can fit something, a **dataset** is which rows and
    // which derived values, a **model** is a dataset plus a provider plus its
    // settings, a **model instance** is one fit of it, and a **prediction** is
    // that fit applied to rows.
    //
    // The dataset has no endpoints of its own, on purpose (§3): it is a JSON
    // column on the model, because a shared named dataset would need a lifecycle
    // — what happens to the four models fitted against it when somebody adds a
    // column — bought for a saving that a Duplicate button answers instead. What
    // it does have is `previewDataset`, which is not a store: it reads the first
    // rows and answers their types, so the builder is a thing you can see the
    // answer of before you fit against it.

    // The providers this build carries, each with the settings and
    // hyperparameters it declares — the same "settings as data" move the trigger
    // form, the file-store picker and the agent-trait form make, so the model
    // form renders a provider it has never heard of.
    //
    // `?dataset=` is the JSON of the dataset being built, and it is what turns a
    // declaration into a form: a provider naming a label has to offer *these*
    // columns as its options (§10), and it cannot know them when it is written.
    // With it, `config_spec` comes back resolved against the dataset's own
    // columns and `outcome` says what a fit would produce; without it the answer
    // is the unresolved declaration, which is what the picker shows before a
    // dataset exists. `?configuration=` is the settings so far, because the
    // outcome is a *function of the configuration* — a random forest is a
    // regressor or a classifier depending on the type of the column its
    // configuration names.
    //
    // An object rather than an array, because the empty list is a real state
    // with a sentence attached: a build made with `--no-default-features` has
    // the two hypothesis tests and nothing else (§13), and an empty picker reads
    // like a bug where "this build was made without them" reads like the
    // decision it is.
    set.register(
        Endpoint::new(
            "listModelProviders",
            Method::Get,
            api().lit("model-providers"),
        )
        // **Text**, holding JSON, rather than a `Json` parameter: a query string
        // carries text, and typing it as JSON would generate a client that
        // stringifies an object with `String()` and sends `[object Object]`.
        // The caller serialises, which is what it is actually doing.
        .query([
            QueryParam::new("dataset", ValueType::Text),
            QueryParam::new("configuration", ValueType::Text),
        ])
        .output(TypeSchema::struct_of([
            StructField::new("providers", TypeSchema::array(model_provider_schema())),
            StructField::new("builtins_compiled_out", TypeSchema::bool()),
            StructField::new("notice", TypeSchema::optional(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Validate a dataset and show what it answers: the column types, and the
    // first rows. A `POST` because the dataset is a document rather than an
    // identifier, and it does not exist anywhere yet — this is the builder
    // asking "is this what I meant?" before there is a model to save.
    //
    // The read is **limited** rather than capped, which is the one place the row
    // bound does not apply: previewing is exactly what an admin does to a
    // dataset that turns out to be too big to fit, and refusing to show it would
    // refuse the answer they came for.
    set.register(
        Endpoint::new(
            "previewDataset",
            Method::Post,
            api().lit("model-datasets").lit("preview"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("dataset", TypeSchema::json()),
            StructField::new("limit", TypeSchema::optional(TypeSchema::int())),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("columns", TypeSchema::array(dataset_column_schema())),
            StructField::new("rows", TypeSchema::array(TypeSchema::json())),
            StructField::new("primary_key", TypeSchema::optional(TypeSchema::text())),
            // Why this dataset cannot be split — a table with a composite or
            // absent primary key reads perfectly well and cannot be fitted (§5),
            // and the builder should say so while it is still being built.
            StructField::new("split_error", TypeSchema::optional(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("listModels", Method::Get, api().lit("models"))
            .query([QueryParam::new("table", ValueType::Text)])
            .output(TypeSchema::array(model_schema()))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "getModel",
            Method::Get,
            api().lit("models").param("id", ValueType::Uuid),
        )
        .output(model_schema())
        .auth(AuthRequirement::admin()),
    );

    // One endpoint for create and replace, rather than the `POST`/`PUT` pair the
    // other records have. A model is edited and refitted continuously and the
    // form always sends the whole definition — there is no partial edit to
    // express — so two endpoints would be one behaviour under two names. The id
    // in the body is what says which: absent is a new model, present is that one
    // replaced.
    set.register(
        Endpoint::new("saveModel", Method::Post, api().lit("models"))
            .input(model_input_schema())
            .output(model_schema())
            .auth(AuthRequirement::admin()),
    );

    // A copy of a model under a new name — the given one, or "… (copy)" —
    // reading the same named datasets, with no fits, and with the original's
    // view state, so it opens laid out as the original was (analytics TODO
    // A3.4–A3.5: the model list's Clone).
    set.register(
        Endpoint::new(
            "cloneModel",
            Method::Post,
            api()
                .lit("models")
                .param("id", ValueType::Uuid)
                .lit("clone"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "name",
            TypeSchema::optional(TypeSchema::text()),
        )]))
        .output(model_schema())
        .auth(AuthRequirement::admin()),
    );

    // What refers to a model by name (analytics TODO A3.5): the calculated
    // fields whose formula calls `predict("…")` on it, and the triggers and
    // workflows that fit it (`fit_model`) or name it in their configuration,
    // and the workspaces whose panels show one of its fits (A4.2).
    // The model list's delete warning lists them; deleting is not refused,
    // since each of them reports the missing model by name when it next runs.
    set.register(
        Endpoint::new(
            "modelUsage",
            Method::Get,
            api()
                .lit("models")
                .param("id", ValueType::Uuid)
                .lit("usage"),
        )
        .output(TypeSchema::struct_of([
            StructField::new(
                "fields",
                TypeSchema::array(TypeSchema::struct_of([
                    StructField::new("table", TypeSchema::text()),
                    StructField::new("field", TypeSchema::text()),
                ])),
            ),
            StructField::new(
                "triggers",
                TypeSchema::array(TypeSchema::struct_of([
                    StructField::new("id", TypeSchema::text()),
                    StructField::new("name", TypeSchema::text()),
                    // `fits` (a `fit_model` step) or `names` (any other mention).
                    StructField::new("how", TypeSchema::text()),
                ])),
            ),
            // The workspaces whose panels show one of its fits (A4.2).
            StructField::new(
                "workspaces",
                TypeSchema::array(crate::analytics::workspace_use_schema()),
            ),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // A model's **view state** (analytics TODO A3.4): the dictionary the
    // screens showing a model keep their layout in — which outputs are open,
    // the optional plots chosen, the selected fit — so that it reopens as it
    // was left. Not part of the model: `saveModel` neither reads nor writes it,
    // a fit does not record it, and nothing about "changed since this fit"
    // looks at it. Patched key by key — a key set to `null` is removed, the
    // others are set, keys not named are left — so two screens keeping
    // different keys do not overwrite each other without a read first. Answers
    // the view state as it now is; `getModel` answers it too.
    set.register(
        Endpoint::new(
            "patchModelViewState",
            Method::Patch,
            api()
                .lit("models")
                .param("id", ValueType::Uuid)
                .lit("view-state"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "patch",
            TypeSchema::json(),
        )]))
        .output(TypeSchema::struct_of([StructField::new(
            "view_state",
            TypeSchema::json(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Deleting a model takes its instances with it, and that is the difference
    // from an agent (whose runs outlive it): an instance is not a record of what
    // happened, it is a fit *of this model* — its coefficients are meaningless
    // without the dataset they were fitted over.
    set.register(
        Endpoint::new(
            "deleteModel",
            Method::Delete,
            api().lit("models").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // **Start** a fit (§8). It answers the instance as soon as the row exists,
    // saying `fitting`, and the work runs on a spawned task — because a fit
    // reads every row of a dataset and runs an optimiser over it, which is
    // seconds at best and minutes at worst, and must not be a request a proxy
    // times out halfway through while the work carries on invisibly. The screen
    // polls `getModelInstance`.
    //
    // A fit of a provider that can stop one (a posterior's chains are
    // processes) is cancelled with `cancelModelFit`; for the others the row cap
    // is the bound that exists instead, since stopping a `smartcore` or a
    // Python call mid-flight is not something the host can do.
    set.register(
        Endpoint::new(
            "fitModel",
            Method::Post,
            api().lit("models").param("id", ValueType::Uuid).lit("fit"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("name", TypeSchema::optional(TypeSchema::text())),
            StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(model_instance_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "listModelInstances",
            Method::Get,
            api()
                .lit("models")
                .param("id", ValueType::Uuid)
                .lit("instances"),
        )
        .output(TypeSchema::array(model_instance_schema()))
        .auth(AuthRequirement::admin()),
    );

    // The instance in full: the parameter blocks, the metrics per split, the
    // grid's scores and the row counts. Separate from the list because the
    // parameters of forty fits are not something a list should carry.
    set.register(
        Endpoint::new(
            "getModelInstance",
            Method::Get,
            api().lit("model-instances").param("id", ValueType::Uuid),
        )
        .output(model_instance_detail_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteModelInstance",
            Method::Delete,
            api().lit("model-instances").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // At most one instance per model is **active**, and that is what lets a
    // formula name a model rather than a fit: the admin refits, activates the
    // new instance, and every `predict("…")` follows without being edited.
    // Activating one deactivates whichever was.
    set.register(
        Endpoint::new(
            "activateModelInstance",
            Method::Post,
            api()
                .lit("model-instances")
                .param("id", ValueType::Uuid)
                .lit("activate"),
        )
        .output(model_instance_schema())
        .auth(AuthRequirement::admin()),
    );

    // Apply a fit to rows, in row order. Either a named `instance` or a `model`
    // (meaning its active instance), and either `rows` typed by the caller — the
    // instance screen's "try a row" box, and a what-if about a row that is not
    // in the table at all — or the model's own dataset, optionally restricted by
    // a `filter` formula.
    //
    // The two are not variations on one thing. Literal rows are whatever the
    // caller typed, because there is nothing to derive them from; dataset rows
    // are read **through the dataset**, so a join path and an aggregation are
    // computed by the row layer exactly as they were at fit time.
    //
    // Admin only, like everything else on this API: an application-facing
    // prediction endpoint is named under *Carried past this milestone*.
    set.register(
        Endpoint::new("predictRows", Method::Post, api().lit("model-predictions"))
            .input(TypeSchema::struct_of([
                StructField::new("model", TypeSchema::optional(TypeSchema::uuid())),
                StructField::new("instance", TypeSchema::optional(TypeSchema::uuid())),
                StructField::new(
                    "rows",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::json())),
                ),
                StructField::new("filter", TypeSchema::optional(TypeSchema::text())),
            ]))
            .output(TypeSchema::struct_of([
                StructField::new("instance", TypeSchema::uuid()),
                StructField::new("outcome", TypeSchema::json()),
                StructField::new("predictions", TypeSchema::array(prediction_schema())),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // --- posteriors ----------------------------------------------------------
    // What a Bayesian model needs beyond the model screens' endpoints (Stan TODO
    // §§5, 13, 16, 18). None of them names Stan: a provider that binds data is
    // one that declares an interface, and a posterior is read, summarised and
    // written back by the host whichever provider sampled it. The two that are
    // about a *program* — checking one and compiling one — are the Stan
    // provider's, because a program is what only it has.

    // Check a program without saving anything: what it declares, and `stanc`'s
    // warnings — or the sentence saying it was not checked, when this server
    // has no CmdStan (§5). A program `stanc` refuses is answered with its
    // diagnostics in `error` rather than as a failed request, because the
    // "Check program" button exists to show them.
    set.register(
        Endpoint::new(
            "getProgramInterface",
            Method::Get,
            api().lit("model-programs"),
        )
        .query([
            QueryParam::new("store", ValueType::Text),
            QueryParam::new("path", ValueType::Text),
        ])
        .output(TypeSchema::struct_of([
            StructField::new("interface", TypeSchema::optional(TypeSchema::json())),
            StructField::new("warnings", TypeSchema::optional(TypeSchema::text())),
            StructField::new("notice", TypeSchema::optional(TypeSchema::text())),
            StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // **Preview data** (§18): the model as the form holds it, bound as far as it
    // binds — each data variable's shape and first values, or its error, on its
    // own row. A `POST` of the whole model for `previewDataset`'s reason: it
    // need not be saved, and usually is not yet.
    set.register(
        Endpoint::new(
            "previewModelData",
            Method::Post,
            api().lit("model-data").lit("preview"),
        )
        .input(model_input_schema())
        .output(TypeSchema::struct_of([
            StructField::new("variables", TypeSchema::array(TypeSchema::json())),
            // The datasets read and bound, the dimensions' sizes, the drops and
            // the warnings of what did bind; null when a sentence about no one
            // variable stopped it, which is then in `errors`.
            StructField::new("report", TypeSchema::optional(TypeSchema::json())),
            StructField::new("errors", TypeSchema::array(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // **Bind automatically** (§18): a binding for each data variable the model
    // leaves unbound that the names, the foreign keys and the size expressions
    // say something about, and why. Nothing bound is replaced; the form fills
    // only its empty rows.
    set.register(
        Endpoint::new(
            "suggestBindings",
            Method::Post,
            api().lit("model-bindings").lit("suggest"),
        )
        .input(model_input_schema())
        .output(TypeSchema::struct_of([
            StructField::new("bindings", TypeSchema::json()),
            StructField::new("reasons", TypeSchema::json()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Warm the compile cache without fitting (§13). It waits for the compile —
    // a minute the first time, nothing after — and answers the cache key, so a
    // compile error is shown where the button was pressed.
    set.register(
        Endpoint::new(
            "compileModel",
            Method::Post,
            api()
                .lit("models")
                .param("id", ValueType::Uuid)
                .lit("compile"),
        )
        .output(TypeSchema::struct_of([
            StructField::new("key", TypeSchema::text()),
            StructField::new("cached", TypeSchema::bool()),
            StructField::new("cmdstan", TypeSchema::text()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Ask a running fit to stop (§13). It sets `cancel_requested` on the row,
    // which the job reads back within a second — so it works from any node,
    // because the row is the registry. Refused by name for a provider whose
    // fit cannot be stopped, and for a fit that has already finished.
    set.register(
        Endpoint::new(
            "cancelModelFit",
            Method::Post,
            api()
                .lit("model-instances")
                .param("id", ValueType::Uuid)
                .lit("cancel"),
        )
        .output(model_instance_schema())
        .auth(AuthRequirement::admin()),
    );

    // One variable's draws, columnar and labelled (§16): per chain, one array
    // per selected element. `elements` is JSON — index arrays, or per axis the
    // keys or labels wanted (`{"counties":["27001"]}`); `chains` is a
    // comma-separated list. An answer of more than `--stan-max-draws-response`
    // numbers is refused with the arithmetic, which `thin` is for.
    set.register(
        Endpoint::new(
            "getModelDraws",
            Method::Get,
            api()
                .lit("model-instances")
                .param("id", ValueType::Uuid)
                .lit("draws"),
        )
        .query([
            QueryParam::new("variable", ValueType::Text),
            QueryParam::new("elements", ValueType::Text),
            QueryParam::new("chains", ValueType::Text),
            QueryParam::new("warmup", ValueType::Bool),
            QueryParam::new("thin", ValueType::Int),
        ])
        .output(TypeSchema::struct_of([
            StructField::new("variable", TypeSchema::text()),
            StructField::new("dims", TypeSchema::array(TypeSchema::int())),
            StructField::new("axes", TypeSchema::array(TypeSchema::text())),
            StructField::new("labels", TypeSchema::array(TypeSchema::json())),
            StructField::new("keys", TypeSchema::array(TypeSchema::json())),
            StructField::new("elements", TypeSchema::array(TypeSchema::json())),
            StructField::new("names", TypeSchema::array(TypeSchema::text())),
            StructField::new("thin", TypeSchema::int()),
            StructField::new("chains", TypeSchema::array(TypeSchema::json())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // The §15 summary of any variable, computed now from its stored draws —
    // including one too large for the fit to have stored a table of. When the
    // draws were not kept, the stored table answers (`source: "stored"`).
    set.register(
        Endpoint::new(
            "getPosteriorSummary",
            Method::Get,
            api()
                .lit("model-instances")
                .param("id", ValueType::Uuid)
                .lit("summary"),
        )
        .query([
            QueryParam::new("variable", ValueType::Text),
            QueryParam::new("elements", ValueType::Text),
        ])
        .output(TypeSchema::struct_of([
            StructField::new("variable", TypeSchema::text()),
            StructField::new("source", TypeSchema::text()),
            StructField::new("columns", TypeSchema::array(TypeSchema::text())),
            StructField::new("elements", TypeSchema::array(TypeSchema::json())),
            StructField::new("names", TypeSchema::array(TypeSchema::text())),
            StructField::new("keys", TypeSchema::array(TypeSchema::json())),
            StructField::new("rows", TypeSchema::array(TypeSchema::json())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // The run as a zip (§16): the raw CmdStan run directory when the model
    // keeps one in a file store, else per-chain draws CSVs built from the table
    // with `coordinates.json`. The response **is** the file — the endpoint model
    // has no bytes shape, so the declared output is empty and the admin UI
    // links to the path rather than calling the client.
    set.register(
        Endpoint::new(
            "downloadModelRun",
            Method::Get,
            api()
                .lit("model-instances")
                .param("id", ValueType::Uuid)
                .lit("run"),
        )
        .output(TypeSchema::json())
        .auth(AuthRequirement::admin()),
    );

    // Write one variable's summary into rows (§16), through the row layer:
    // `update` into the rows of the table its one axis is about, matched by
    // key; `insert`, one row per element into `table`. The same write-back a
    // code body's model handle makes (`sc_api::models::write_posterior`).
    set.register(
        Endpoint::new(
            "writePosterior",
            Method::Post,
            api()
                .lit("model-instances")
                .param("id", ValueType::Uuid)
                .lit("posterior-writes"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("variable", TypeSchema::text()),
            StructField::new("mode", TypeSchema::text()),
            StructField::new("statistics", TypeSchema::json()),
            StructField::new("table", TypeSchema::optional(TypeSchema::text())),
            StructField::new("coordinates", TypeSchema::optional(TypeSchema::json())),
            StructField::new("instance_field", TypeSchema::optional(TypeSchema::text())),
            StructField::new("elements", TypeSchema::optional(TypeSchema::json())),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("variable", TypeSchema::text()),
            StructField::new("mode", TypeSchema::text()),
            StructField::new("table", TypeSchema::text()),
            StructField::new("instance", TypeSchema::text()),
            StructField::new("written", TypeSchema::int()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // --- streams ------------------------------------------------------------
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
            "listStreamProviders",
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
        ]))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("listStreams", Method::Get, api().lit("streams"))
            .output(TypeSchema::array(stream_schema()))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "getStream",
            Method::Get,
            api().lit("streams").param("id", ValueType::Uuid),
        )
        .output(stream_schema())
        .auth(AuthRequirement::admin()),
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
        Endpoint::new("saveStream", Method::Post, api().lit("streams"))
            .input(stream_input_schema())
            .output(stream_schema())
            .auth(AuthRequirement::admin()),
    );

    // Refused while a trigger names this stream as its channel, listing them —
    // the refusal `delete_llm_model` already makes, for the same reason: the
    // reference is by name, so deleting the stream would leave a trigger
    // listening to a channel nothing will ever raise.
    set.register(
        Endpoint::new(
            "deleteStream",
            Method::Delete,
            api().lit("streams").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // How it is going **right now**: the one endpoint a stream needs that a
    // model does not. A stream's state — connected, retrying since when, how
    // many elements — is held in memory by the supervisor and is deliberately
    // not a column (§6), so it cannot be read back from the row, and the
    // Streams list polls this rather than re-reading definitions it already
    // has.
    set.register(
        Endpoint::new(
            "streamStatus",
            Method::Get,
            api()
                .lit("streams")
                .param("id", ValueType::Uuid)
                .lit("status"),
        )
        .output(stream_status_schema())
        .auth(AuthRequirement::admin()),
    );

    // --- settings -----------------------------------------------------------
    // The `_fd_config` values an admin edits (§9, §13.5). Two endpoints, and
    // both carry the **declarations** alongside the values, for the same reason
    // the file-store and LLM-provider screens are handed a `config_spec`: the
    // settings screen renders whatever the server declares and knows nothing
    // about any particular setting. Adding one is a Rust declaration and a
    // redeployed server, with no matching change in the SPA.
    //
    // The save returns the settings as they now stand rather than an
    // acknowledgement, so the screen shows what was stored — including the
    // defaults a cleared box fell back to, and the sentinel standing in for a
    // secret it must not be handed back.
    set.register(
        Endpoint::new("getSettings", Method::Get, api().lit("settings"))
            .output(settings_schema())
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("updateSettings", Method::Post, api().lit("settings"))
            .input(TypeSchema::struct_of([StructField::new(
                "values",
                TypeSchema::json(),
            )]))
            .output(settings_schema())
            .auth(AuthRequirement::admin()),
    );

    // Send one message through the **stored** email settings (§18.2).
    //
    // A section may have an *act* as well as fields, and this is the first one:
    // "are these settings right" is a question only the network can answer, and
    // an admin who has to wait for a trigger to fire to find out has no way to
    // tell a wrong password from a wrong template.
    //
    // It tests what is **saved**, not what is typed — the transport is built
    // from `_fd_config` — so the answer is about the configuration this
    // installation will actually send with. `to` defaults to the signed-in
    // admin's own address, because the admin pressing the button is the one
    // person guaranteed to be able to check whether it arrived.
    set.register(
        Endpoint::new(
            "sendTestEmail",
            Method::Post,
            api().lit("settings").lit("email").lit("test"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "to",
            TypeSchema::optional(TypeSchema::text()),
        )]))
        .output(TypeSchema::struct_of([StructField::new(
            "sent_to",
            TypeSchema::text(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // **Python, as this process has it** (design §15; TODO "The Python code
    // adapter" §7). Read-only, on the Development tab, and none of it is a
    // setting: "why does my Python trigger not work" has three answers — not
    // built with Python, built but told not to start one, running — and every
    // number beside them is a fact about the running process rather than
    // something an admin types. `explanation` is the server's own sentence for
    // whichever state it is in, because which one is true depends on how this
    // binary was built and how it was started.
    set.register(
        Endpoint::new("getPythonStatus", Method::Get, api().lit("python"))
            .output(TypeSchema::struct_of([
                StructField::new("state", TypeSchema::text()),
                StructField::new("version", TypeSchema::optional(TypeSchema::text())),
                StructField::new("explanation", TypeSchema::text()),
                // Where the environment is, where a package lands inside it, and
                // which external interpreter builds it (§9). All optional: an
                // interpreter that has not started cannot say which directory
                // its packages would come from, because that depends on its own
                // version.
                StructField::new("dir", TypeSchema::optional(TypeSchema::text())),
                StructField::new("site_packages", TypeSchema::optional(TypeSchema::text())),
                StructField::new("bin", TypeSchema::optional(TypeSchema::text())),
                StructField::new(
                    "packages",
                    TypeSchema::array(TypeSchema::struct_of([
                        StructField::new("name", TypeSchema::text()),
                        StructField::new("version", TypeSchema::optional(TypeSchema::text())),
                    ])),
                ),
                // The admission bound and what is against it, then the leak:
                // a thread that never came back cannot be reclaimed, so the
                // count is here to be seen long before it reaches its limit.
                StructField::new("max_inflight", TypeSchema::int()),
                StructField::new("resident", TypeSchema::int()),
                StructField::new("threads", TypeSchema::int()),
                StructField::new("stuck", TypeSchema::int()),
                StructField::new("max_stuck", TypeSchema::int()),
                // Why the environment is not in use, where that is the case
                // (§9): a virtual environment built by a different Python holds
                // packages this interpreter cannot import — a C extension would
                // crash the server rather than fail to import — so it is left
                // off `sys.path` and this says so, naming both versions.
                StructField::new("env_error", TypeSchema::optional(TypeSchema::text())),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // --- backup & restore ---------------------------------------------------
    // Two of the four backup operations are here; the other two are routes outside
    // this set, because one *is* a file and the other *takes* one, and a
    // `TypeSchema` has no bytes shape (the same reason the binary file upload is
    // outside it). The split is along that line and no other: what an admin
    // includes, and what a restore did, are ordinary typed JSON and belong in the
    // generated client.

    // What this server has to offer a backup, and the selection the admin last
    // made — one response, because the dialog needs both to open and asking for
    // them separately would let them disagree.
    set.register(
        Endpoint::new("getBackupOptions", Method::Get, api().lit("backup"))
            .output(TypeSchema::struct_of([
                StructField::new("available", backup_contents_schema()),
                StructField::new("include", backup_selection_schema()),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // Restore from an archive already uploaded (`/backup/upload` handed back its
    // `id`), taking the parts `include` names.
    //
    // The two lists rather than a single "ok": a restore is dozens of independent
    // acts and some of them are routinely skipped — an account that is already
    // here, a trigger on a table the admin left out. Reporting that as success
    // would hide it and as failure would be wrong.
    set.register(
        Endpoint::new(
            "restoreBackup",
            Method::Post,
            api().lit("backup").lit("restore"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("id", TypeSchema::text()),
            StructField::new("include", backup_selection_schema()),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("restored", TypeSchema::array(TypeSchema::text())),
            StructField::new("warnings", TypeSchema::array(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // **Clear all**: back to an empty installation (Settings → Development).
    //
    // Two calls because the dialog asks a question first: every file store and
    // the directory it occupies, so the admin can choose which ones leave the
    // disk as well as the database. `delete_from_disk` names stores by name; a
    // store left out keeps its files where they are.
    set.register(
        Endpoint::new("getClearAllPreview", Method::Get, api().lit("clear-all"))
            .output(TypeSchema::struct_of([StructField::new(
                "file_stores",
                TypeSchema::array(TypeSchema::struct_of([
                    StructField::new("name", TypeSchema::text()),
                    StructField::new("backend", TypeSchema::text()),
                    StructField::new("directory", TypeSchema::optional(TypeSchema::text())),
                ])),
            )]))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("clearAll", Method::Post, api().lit("clear-all"))
            .input(TypeSchema::struct_of([StructField::new(
                "delete_from_disk",
                TypeSchema::array(TypeSchema::text()),
            )]))
            .output(TypeSchema::struct_of([
                StructField::new("cleared", TypeSchema::array(TypeSchema::text())),
                StructField::new("warnings", TypeSchema::array(TypeSchema::text())),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // The Analytics UI's datasets and workspaces (analytics TODO A1.13).
    crate::analytics::register(&mut set);

    set
}

/// The six flags a token carries, which are the `admin_copilot` agent's six
/// flags (§13.6) — one vocabulary for "what may this agent do to my
/// installation?", whether the agent is the built-in copilot or an external one
/// reached over MCP.
///
/// Every field is optional on the way *in* and present on the way *out*: a
/// client may leave a flag to its default, and a stored credential records what
/// was actually agreed to rather than what a later default would say.
fn api_token_grants_schema() -> TypeSchema {
    TypeSchema::struct_of(
        crate::mcp::FLAG_KEYS
            .into_iter()
            .map(|key| StructField::new(key, TypeSchema::optional(TypeSchema::bool()))),
    )
}

/// One stored API token, as a list sees it.
///
/// There is no field for the token and none for its hash, which is the same
/// omission `sc_auth::ApiToken` makes and for the same reason: "shown once" is a
/// property of the shape rather than of everyone's care.
fn api_token_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("user_id", TypeSchema::uuid()),
        StructField::new("label", TypeSchema::text()),
        // Every flag present and explicit — the record of what was agreed to.
        StructField::new("grants", TypeSchema::json()),
        StructField::new("created_at", TypeSchema::timestamp()),
        StructField::new("expires_at", TypeSchema::optional(TypeSchema::timestamp())),
        StructField::new(
            "last_used_at",
            TypeSchema::optional(TypeSchema::timestamp()),
        ),
        StructField::new("revoked_at", TypeSchema::optional(TypeSchema::timestamp())),
        // Whether it would authenticate right now as far as the row can tell —
        // neither revoked nor lapsed. The screen draws a badge from this rather
        // than recomputing the clock arithmetic in TypeScript.
        StructField::new("live", TypeSchema::bool()),
    ])
}

/// One thing a backup can include or leave out: what it is called, what to show,
/// and how much of it there is (rows or files; `null` where counting it would cost
/// more than the number is worth).
fn backup_item_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("count", TypeSchema::optional(TypeSchema::int())),
    ])
}

/// Everything that could go into a backup — of this server, or of a backup file
/// that has been uploaded. **One schema for both**, which is what lets one dialog
/// drive the backup and the restore: the difference between them is where the
/// value came from, not what it is.
fn backup_contents_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("tables", TypeSchema::array(backup_item_schema())),
        StructField::new("applications", TypeSchema::array(backup_item_schema())),
        StructField::new("file_stores", TypeSchema::array(backup_item_schema())),
        // Counts rather than flags, because "back up the users" is a different
        // decision when there are two of them and when there are twelve thousand.
        StructField::new("users", TypeSchema::int()),
        StructField::new("modules", TypeSchema::int()),
        StructField::new("db_connections", TypeSchema::int()),
        StructField::new("streams", TypeSchema::int()),
        // The Analytics choice carries all three; they are counted apart so
        // the dialog can say what is in it.
        StructField::new("datasets", TypeSchema::int()),
        StructField::new("models", TypeSchema::int()),
        StructField::new("workspaces", TypeSchema::int()),
        // Fitted model instances, with their draws and output frames: a
        // choice of their own because they can outweigh everything else.
        StructField::new("fits", TypeSchema::int()),
        StructField::new("llm_providers", TypeSchema::int()),
        StructField::new("agents", TypeSchema::int()),
        StructField::new("triggers", TypeSchema::int()),
        // The views and pages of every application on offer. They travel with
        // their application: a view restored without the application it belongs
        // to has nowhere to go.
        StructField::new("views", TypeSchema::int()),
        StructField::new("pages", TypeSchema::int()),
        StructField::new("ssl", TypeSchema::bool()),
        // Every other settings section: email, localisation, development.
        StructField::new("settings", TypeSchema::bool()),
    ])
}

/// What one backup or restore includes.
///
/// `table_data` is separate from `tables` because metadata and rows are separate
/// decisions — a schema-only backup is a real thing to want — and it is a **subset**
/// of it: rows restored into a table nobody described would be unreadable, so the
/// server narrows this to `tables` on the way in rather than trusting it.
fn backup_selection_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("tables", TypeSchema::array(TypeSchema::text())),
        StructField::new("table_data", TypeSchema::array(TypeSchema::text())),
        StructField::new("applications", TypeSchema::array(TypeSchema::text())),
        StructField::new("file_stores", TypeSchema::array(TypeSchema::text())),
        StructField::new("users", TypeSchema::bool()),
        StructField::new("modules", TypeSchema::bool()),
        StructField::new("db_connections", TypeSchema::bool()),
        StructField::new("streams", TypeSchema::bool()),
        StructField::new("analytics", TypeSchema::bool()),
        // Only with `analytics`, which the server enforces as it does rows
        // with their table.
        StructField::new("fits", TypeSchema::bool()),
        StructField::new("llm_providers", TypeSchema::bool()),
        StructField::new("agents", TypeSchema::bool()),
        StructField::new("triggers", TypeSchema::bool()),
        StructField::new("views", TypeSchema::bool()),
        StructField::new("pages", TypeSchema::bool()),
        StructField::new("ssl", TypeSchema::bool()),
        StructField::new("settings", TypeSchema::bool()),
    ])
}

/// The most rows one `listRows` will answer with.
///
/// A ceiling rather than a page size: the grid picks its own page and the number
/// it sends is clamped to this, so a caller who asks for the whole of a million-
/// row table gets a page and not the table. It is generous because the admin
/// grid is the one surface that legitimately pages fast — a screen of rows plus
/// a screen of overscan either side.
pub const ROW_PAGE_CAP: u64 = 1_000;

/// The query string a row read takes: the shared vocabulary
/// (`crate::query_string`) minus `select`, which is the REST provider's own
/// shape and has no meaning for a grid that shows the table's own columns.
fn row_read_params() -> Vec<QueryParam> {
    vec![
        QueryParam::new("order", ValueType::Text),
        QueryParam::new("limit", ValueType::Int),
        QueryParam::new("offset", ValueType::Int),
        QueryParam::new("filter", ValueType::Text).map(),
    ]
}

/// A `PathSpec` rooted at the admin API prefix.
fn api() -> PathSpec {
    PathSpec::root().lit(ADMIN_API_PREFIX)
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
    // Which database hosts the table: `primary`, or the name of the connection
    // it came from (§5's Connections). Reported on every table rather than only
    // on foreign ones, so a client never has to read absence as "the primary".
    fields.push(StructField::new("database", TypeSchema::text()));
    // Whether this is one of Saltcorn's own metadata tables, added to the list:
    // its rows and settings are editable, its fields and constraints are not.
    fields.push(StructField::new("metadata", TypeSchema::bool()));
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

/// One role (technical design §7.1, §9).
///
/// **Why roles are rows and not a constant list.** Roles are the fixed scale
/// `1..=100`, and for a while an integer was all a role was: `1` meant admin
/// because a constant said so, `100` meant public, and the ninety-eight numbers
/// between meant whatever an installation's users made them mean. That stops
/// working the moment a role has to *carry* something — a name to show in a
/// pick-list, settings that apply to everyone holding it — because a row can
/// carry those and an integer cannot. So `_fd_roles` holds them, `users.role`
/// references it, and this endpoint reports what is there rather than what a
/// constant asserts.
///
/// `builtin` marks the two the system itself depends on (admin and public):
/// they are not deletable, and the UI has to know that before offering the
/// button rather than after refusing the request.
fn role_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("role", TypeSchema::int()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("builtin", TypeSchema::bool()),
    ])
}

/// The body of a user create or edit — the same shape for both, since a user
/// form is a user form.
///
/// `password` is optional and its blank means two different things by design, one
/// per operation: **on create** it asks for a generated password (returned once,
/// in the response), and **on update** it leaves the stored hash alone. The
/// alternative — making the admin type a password to change a role — is what
/// makes people reuse one.
///
/// `extra` carries the columns the admin has added to the users table (§7.1),
/// keyed by column name; the system's own columns are refused there, since each
/// has its own way in.
/// What languages this installation serves: the default, and the enabled set
/// (§16.1). `enabled` always contains `default`, and a one-element `enabled` is
/// an installation that negotiates nothing.
fn locales_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("default", TypeSchema::text()),
        StructField::new("enabled", TypeSchema::array(TypeSchema::text())),
        // The locale **this request** was negotiated into (§16.1, D8), so the
        // SPA loads the catalogue the server has already committed to in
        // `Content-Language` rather than negotiating a second time from the
        // browser's own idea of the order. Two negotiations of one request is
        // how a page ends up with a French navbar and English tables.
        StructField::new("current", TypeSchema::text()),
    ])
}

fn user_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("email", TypeSchema::text()),
        StructField::new("password", TypeSchema::optional(TypeSchema::text())),
        StructField::new("role", TypeSchema::int()),
        // A BCP-47 tag, or null/absent for "whatever the request negotiates".
        // On an **update** the two are not the same: an absent `language` leaves
        // the stored one alone, and an explicit `null` clears it, which is what
        // the form's "Site default" option sends (§16.1).
        StructField::new("language", TypeSchema::optional(TypeSchema::text())),
        StructField::new("extra", TypeSchema::optional(TypeSchema::json())),
    ])
}

/// A field (column) of a table, with the `_fd_fields` overlay merged onto it
/// (§3.2): the introspected `sql_type`/`nullable`/`unique`, plus the overlay's
/// `type` (a rich type's name, or the basic type's), `kind` (with its
/// parameters), `label`, `description` and `attributes`.
fn field_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("sql_type", TypeSchema::text()),
        StructField::new("type", TypeSchema::text()),
        StructField::new("nullable", TypeSchema::bool()),
        StructField::new("required", TypeSchema::bool()),
        StructField::new("unique", TypeSchema::bool()),
        StructField::new("primary_key", TypeSchema::bool()),
        // Whether the column fills itself in when a write omits it. A fact about
        // the column read back by introspection, not a wish recorded when it was
        // created, so it stays true however the field came to be a key.
        StructField::new("generated", TypeSchema::bool()),
        // `kind` and `attributes` are opaque JSON: their shape depends on the
        // field's kind and type, which the API cannot know statically any more
        // than it can a framework's settings.
        StructField::new("kind", TypeSchema::json()),
        StructField::new("attributes", TypeSchema::json()),
    ])
}

/// The body accepted when **creating** a field. `type` is a basic-type or
/// registered-rich-type name — `sql_type` is derived from it, not asked for, so
/// the two can never disagree (§3.3). Everything past `name` is optional:
/// `type` may be omitted for a `Key`, whose storage type is its target's and so
/// is not the caller's to choose (`schema_edit::FieldSpec`).
fn create_field_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("type", TypeSchema::optional(TypeSchema::text())),
        StructField::new("kind", TypeSchema::optional(TypeSchema::json())),
        StructField::new("attributes", TypeSchema::optional(TypeSchema::json())),
        StructField::new("label", TypeSchema::optional(TypeSchema::text())),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("required", TypeSchema::optional(TypeSchema::bool())),
        StructField::new("unique", TypeSchema::optional(TypeSchema::bool())),
        // The key is a field like any other (GOALS): a table is created with no
        // primary key at all, and gets one when a field says it is one. More
        // than one field may, and then the key is composite in field order.
        StructField::new("primary_key", TypeSchema::optional(TypeSchema::bool())),
    ])
}

/// One constraint on a table (§5): what kind it is, what it names, and the
/// message its violation is reported with.
///
/// `name` is the database object's own name — the constraint's, the index's or
/// the trigger's — because that is what a violation reports and what a delete
/// addresses. It is **derived** on create rather than asked for (see
/// `TableConstraint::derived_name`), so the screen shows a name the admin did
/// not have to invent and adding the same rule twice collides by name.
fn constraint_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        // `unique` | `index` | `full_text_search` | `formula`.
        StructField::new("type", TypeSchema::text()),
        StructField::new("fields", TypeSchema::array(TypeSchema::text())),
        StructField::new("expression", TypeSchema::optional(TypeSchema::text())),
        StructField::new("method", TypeSchema::optional(TypeSchema::text())),
        StructField::new("language", TypeSchema::optional(TypeSchema::text())),
        StructField::new("formula", TypeSchema::optional(TypeSchema::text())),
        StructField::new("error_message", TypeSchema::optional(TypeSchema::text())),
        // Whether Saltcorn created it, which is what the screen needs to know
        // before offering to delete a constraint somebody else's migration owns.
        StructField::new("managed", TypeSchema::bool()),
    ])
}

/// The body that adds a constraint. One shape for four kinds, because the
/// alternative is four endpoints whose bodies differ by two fields — and the
/// kind decides which of them are read, which the handler says by name when one
/// is missing.
fn create_constraint_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("type", TypeSchema::text()),
        // Jointly-unique: the fields that are unique *together*.
        StructField::new(
            "fields",
            TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
        ),
        // Full-text search: the text-search configuration.
        StructField::new("language", TypeSchema::optional(TypeSchema::text())),
        // A row constraint: the formula, and the short name it is known by —
        // the one kind whose identity cannot be derived from its fields.
        StructField::new("formula", TypeSchema::optional(TypeSchema::text())),
        StructField::new("name", TypeSchema::optional(TypeSchema::text())),
        StructField::new("error_message", TypeSchema::optional(TypeSchema::text())),
    ])
}

/// The body accepted when **editing** a field — the overlay-only subset, plus
/// the two column properties that must be reachable after the fact. No `name`,
/// `unique` or storage type: those are the database's, and changing them is a
/// schema change out of scope for this milestone.
///
/// `primary_key` is one exception, and a considered one: since no table is
/// created with a key it did not declare, a table that has none — imported from
/// a CSV with no key column, or built a field at a time — could otherwise only
/// get one by being dropped and recreated with its rows thrown away.
///
/// `required` is the other: whether a field may be left empty is a rule about
/// the data that changes as an application does. Making one required is refused
/// while a row has no value in it; a key field is required whatever this says.
/// Both are **omitted means leave it**, unlike the rest of this body.
fn field_settings_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("type", TypeSchema::optional(TypeSchema::text())),
        StructField::new("kind", TypeSchema::optional(TypeSchema::json())),
        StructField::new("attributes", TypeSchema::optional(TypeSchema::json())),
        StructField::new("label", TypeSchema::optional(TypeSchema::text())),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("primary_key", TypeSchema::optional(TypeSchema::bool())),
        StructField::new("required", TypeSchema::optional(TypeSchema::bool())),
    ])
}

/// One entry of `listFieldTypes`: a basic type, a rich type, or a field kind,
/// each with the `config_spec` its attribute form is rendered from (empty for a
/// basic type). `category` lets the editor group them; `name` is what
/// `createField`/`updateField` take as `type` (basic/rich) or `kind.type` (kind).
fn field_type_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("category", TypeSchema::text()),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
    ])
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

/// The fields common to a file store on the wire — everything but its id.
///
/// `config` is opaque JSON: it is whatever the chosen backend's `config_spec`
/// declares, and the API cannot know that statically any more than it can know a
/// framework's settings (see [`framework_ref_schema`]).
fn file_store_fields() -> Vec<StructField> {
    vec![
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("backend", TypeSchema::text()),
        StructField::new("config", TypeSchema::json()),
        // Null means unrestricted, which is distinct from any particular role.
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
    ]
}

/// A file store as reported to the admin UI: its definition, plus the live state
/// that only the running server knows.
///
/// The trailing fields are why a store is more than its row. `connected` and
/// `error` exist because a definition can be perfectly valid and still not
/// usable — a directory unmounted since it was saved — and the UI has to show
/// that state with its reason rather than silently omitting the store or
/// pretending it works. `is_git_repo` is a property of the connected instance,
/// so it is null when there is no instance to ask.
///
/// **`id` is nullable, and that is the interesting case.** A store connected by
/// the `--file-store` flag is real and usable but has no row (§1.3: the flag is
/// deliberately ephemeral), so it has no id. Listing only stored definitions
/// would hide it — a developer running with the flag would see an empty store
/// list and no store to browse — so the listing is the *union* of defined and
/// connected stores. A null id is precisely what tells the UI that a store
/// cannot be edited or deleted: there is no row to edit, and it will be gone on
/// the next boot unless the flag is passed again.
fn file_store_schema() -> TypeSchema {
    let mut fields = vec![StructField::new(
        "id",
        TypeSchema::optional(TypeSchema::uuid()),
    )];
    fields.extend(file_store_fields());
    fields.extend([
        StructField::new("connected", TypeSchema::bool()),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        StructField::new("is_git_repo", TypeSchema::optional(TypeSchema::bool())),
    ]);
    TypeSchema::Struct(fields)
}

/// The body accepted when creating or updating a file store: the definition's
/// fields minus the id (server-assigned on create, taken from the path on
/// update) and minus the live state, which is observed rather than set.
fn file_store_input_schema() -> TypeSchema {
    TypeSchema::Struct(file_store_fields())
}

/// One file's metadata (design §9): the access rule and the free-form attributes
/// kept beside the bytes rather than in a database row.
///
/// `effective_min_role` is the *computed* answer — the most restrictive rule on
/// the whole path, including the store's own floor and every parent directory —
/// while `min_role` is only what is set on this entry. The UI needs both: the
/// second is what an admin edits, the first is what actually applies, and
/// showing only the second would let an admin believe a file is public when a
/// parent directory has locked it.
fn file_meta_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("path", TypeSchema::text()),
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
        StructField::new(
            "effective_min_role",
            TypeSchema::optional(TypeSchema::int()),
        ),
        StructField::new("attributes", TypeSchema::json()),
    ])
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
/// `id` is not optional here, unlike a file store's: there is no `--llm-provider`
/// flag and no such thing as a provider without a row, so every provider in a
/// listing is one that can be edited and deleted.
///
/// There is no `connected` either, and its absence is the design: connecting a
/// provider builds an HTTP client and sends nothing, so "connected" would be a
/// word for "the configuration parsed" — which the admin already knows, because
/// the save succeeded. Whether a model *works* is a request, and that is what
/// `testLlmModel` is.
fn llm_provider_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("id", TypeSchema::uuid())];
    fields.extend(llm_provider_fields());
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
        // At most one per provider: saving one as the default clears the rest.
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

/// One installed module, as the Modules tab reads it: the row, what the package
/// turned out to supply, and everything wrong with it.
///
/// `configuration` is **redacted** (§11.1): a module's `password` field is a
/// secret like any other, so what crosses the wire is the sentinel and a save
/// that returns it unchanged keeps what is stored.
fn module_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        // `javascript` or `python`: which host loads it, which package manager
        // installed it, and whether `permissions` applies to it at all (§10).
        StructField::new("language", TypeSchema::text()),
        StructField::new("source", TypeSchema::text()),
        StructField::new("location", TypeSchema::text()),
        StructField::new("version", TypeSchema::optional(TypeSchema::text())),
        StructField::new("configuration", TypeSchema::json()),
        // What its worker may reach (§2): `{ net, read, write, env }`, every
        // one an allow-list and every empty one meaning *nothing*. Closed
        // unless an admin granted something, and reported here so the tab can
        // say what a module can do as well as what it supplies.
        StructField::new("permissions", TypeSchema::json()),
        // The module's own settings, from its `configuration_workflow` — the
        // same declaration a file store's backend sends, so the form is
        // generic.
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
        StructField::new("actions", TypeSchema::array(module_action_schema())),
        // The functions it supplies (§4a): what a code body calls through
        // `modfn` and what a formula hoists, with the signature v1 declared —
        // which is what the code editor's generated types read.
        StructField::new("functions", TypeSchema::array(module_function_schema())),
        // The table providers it supplies (§8.3) — names only, because what a
        // provider *asks for* belongs to the table being created and is on
        // `listTableProviders`.
        StructField::new("table_providers", TypeSchema::array(TypeSchema::text())),
        // The model providers it supplies — names only, for the same reason:
        // what a provider *asks for* belongs to the model being fitted and is
        // on `listModelProviders`.
        StructField::new("model_providers", TypeSchema::array(TypeSchema::text())),
        // The stream providers it supplies (TODO "Streams" §12) — names only,
        // for the same reason again: what one *asks for* belongs to the stream
        // being created and is on `listStreamProviders`.
        StructField::new("stream_providers", TypeSchema::array(TypeSchema::text())),
        // The view patterns it supplies (TODO "Saltcorn UI" 11.1) — names only:
        // what one *asks for* is its configuration wizard, a call per step on
        // `viewConfigStep`.
        StructField::new("view_patterns", TypeSchema::array(TypeSchema::text())),
        // What it also supplies and this version does not load: `{key, count}`,
        // so the tab can say "also supplies 1 table provider (not yet
        // supported)".
        StructField::new(
            "unsupported",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("key", TypeSchema::text()),
                StructField::new("count", TypeSchema::optional(TypeSchema::int())),
            ])),
        ),
        StructField::new("issues", TypeSchema::array(TypeSchema::text())),
        // Whether the package loaded at all. A module with `loaded: false`
        // supplies nothing and its `issues` say why.
        StructField::new("loaded", TypeSchema::bool()),
        StructField::new("api_version", TypeSchema::optional(TypeSchema::int())),
    ])
}

/// One entry in the **bundled catalog**: a module this server ships with.
///
/// Not a `module_schema` with fields left empty. A bundled module that has not
/// been installed has no row, no version, no configuration and supplies nothing
/// — what it has is a card: a heading, a sentence, what it would give, and what
/// installing it would download and grant. `name` is the link between the two,
/// because that is what the installed row is keyed by: a client shows an entry
/// as installed when a module in the same response carries the same name.
fn bundled_module_schema() -> TypeSchema {
    TypeSchema::struct_of([
        // The catalog id — `rss` — which is what `installModule` is given as
        // `location` for a `bundled` source.
        StructField::new("id", TypeSchema::text()),
        // The package's own name once installed — `@feldspar/rss`.
        StructField::new("name", TypeSchema::text()),
        StructField::new("language", TypeSchema::text()),
        StructField::new("title", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // What it supplies, one sentence each, written for the card rather than
        // read from the package: nothing has been installed, so there is no
        // package to read.
        StructField::new("supplies", TypeSchema::array(TypeSchema::text())),
        // What installing it downloads — the dependencies that are deliberately
        // not in the release, so an admin knows the click reaches a registry.
        StructField::new("installs", TypeSchema::array(TypeSchema::text())),
        // What it will be granted: the same `{ net, read, write, env }` shape a
        // module's own permissions have, so one renderer draws both. Always
        // closed for a Python module — there is nothing to enforce it (§10).
        StructField::new("permissions", TypeSchema::json()),
        // Whether a module with this name is already installed, which is what
        // decides whether the card carries a button or a tick.
        StructField::new("installed", TypeSchema::bool()),
    ])
}

/// One function a module supplies, with the signature v1 declared for it.
///
/// `is_async` is v1's own `isAsync` and decides nothing about how the function
/// is called — everything crosses the host seam awaited. It is here because it
/// is what a signature in the code editor says, and it is v1's word.
fn module_function_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("is_async", TypeSchema::bool()),
        StructField::new(
            "arguments",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                // v1's type name (`String`, `Integer`, `Object`), absent when
                // the module declared none — a guess would read as a promise.
                StructField::new("type", TypeSchema::optional(TypeSchema::text())),
            ])),
        ),
    ])
}

/// One action a module supplies, with the settings it declares.
fn module_action_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
    ])
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

/// One stored agent, with the reason it cannot run when there is one.
///
/// `error` is what makes a broken agent fixable rather than merely absent, as a
/// trigger's and a file store's are: an agent naming a provider that was deleted
/// or a trait configured against a dropped table is **not in the live set** and
/// will not answer, but it is still stored, still listed and still editable.
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

/// One run in a list: everything except the transcript.
///
/// The transcript is deliberately absent. A chat's history is a list of dozens
/// of runs and each `context` is a whole conversation, so a list carrying them
/// would send megabytes to render a sidebar; [`run_schema`] is what the panel
/// asks for when a run is opened.
fn run_summary_schema() -> TypeSchema {
    TypeSchema::Struct(vec![
        StructField::new("id", TypeSchema::uuid()),
        // `agent` or `workflow` (§10.3's engine shares this table).
        StructField::new("kind", TypeSchema::text()),
        // What the run is of: the agent's or the trigger's **name**.
        StructField::new("subject", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // `running` | `waiting` | `done` | `failed` | `aborted`.
        StructField::new("state", TypeSchema::text()),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        StructField::new("user", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("created_at", TypeSchema::timestamp()),
        StructField::new("updated_at", TypeSchema::timestamp()),
        // The workflow half (§10.3), null on an agent run. The version this run
        // is **pinned** to is the whole of "a suspended run finishes on its own
        // version", and a list that did not show it would hide the one fact that
        // explains why two runs of one workflow behaved differently.
        StructField::new("subject_version", TypeSchema::optional(TypeSchema::int())),
        // The step it is on, or null once it is over.
        StructField::new("current_step", TypeSchema::optional(TypeSchema::text())),
        // When it next wants the engine. Null and `waiting` together mean "only a
        // person can wake this", which is what the run list has to be able to
        // show as an approval somebody is sitting on.
        StructField::new("wake_at", TypeSchema::optional(TypeSchema::timestamp())),
        // How an agent run's loop concluded, as the loop stores it:
        // `{"conclusion": "answered" | "max_steps" | "aborted" | "over_budget" |
        // "stuck", "budget"?, "reason"?}`. `done` covers an answer, a budget
        // and a stuck run alike, and this is what tells them apart. Null while
        // running and for a workflow run.
        StructField::new("conclusion", TypeSchema::optional(TypeSchema::json())),
        // The run that delegated this one — set on a subagent's run, null on a
        // run somebody started. The chat history lists only the latter: a
        // child's transcript is read nested inside its parent's.
        StructField::new("parent_run", TypeSchema::optional(TypeSchema::uuid())),
    ])
}

/// One whole run: the summary plus the state it can be read back from, and — for
/// a workflow run — its trace and the form it is waiting on.
///
/// `context` is passed through as JSON rather than described field by field: it
/// is `sc-agent`'s `AgentLoop` or `sc-workflow`'s `WorkflowRun`, whose shape
/// belongs to the engine and changes with it, and a second declaration of it here
/// would be a second thing to keep in step. What the chat panel reads out of it —
/// the messages — is stable, and so is what the run detail reads: the trace and
/// the pending form are lifted out into fields of their own below.
fn run_schema() -> TypeSchema {
    let TypeSchema::Struct(mut fields) = run_summary_schema() else {
        return run_summary_schema();
    };
    fields.push(StructField::new("context", TypeSchema::json()));
    fields.push(StructField::new("attributes", TypeSchema::json()));
    // The workflow half. Empty and null on an agent run rather than absent, so
    // one typed shape serves both and the client has no union to narrow.
    fields.push(StructField::new(
        "trace",
        TypeSchema::array(run_trace_schema()),
    ));
    fields.push(StructField::new(
        "pending_form",
        TypeSchema::optional(pending_form_schema()),
    ));
    // A planner run's plan, read out of its `coding` state (TODO §8), so the
    // chat can show the checklist without knowing where a trait keeps it. Null
    // on every other run.
    fields.push(StructField::new(
        "plan",
        TypeSchema::optional(run_plan_schema()),
    ));
    TypeSchema::Struct(fields)
}

/// A planner run's plan: its features, and one progress entry per session.
fn run_plan_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new(
            "features",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("id", TypeSchema::text()),
                StructField::new("title", TypeSchema::text()),
                StructField::new("description", TypeSchema::text()),
                // `feature` | `bug`.
                StructField::new("kind", TypeSchema::text()),
                StructField::new("acceptance", TypeSchema::array(TypeSchema::text())),
                StructField::new("files", TypeSchema::array(TypeSchema::text())),
                StructField::new("pages", TypeSchema::array(TypeSchema::text())),
                // `todo` | `in_progress` | `done` | `failed` | `blocked`.
                StructField::new("status", TypeSchema::text()),
                StructField::new("attempts", TypeSchema::int()),
                // Its sessions' run ids, latest last: what the checklist links to.
                StructField::new("runs", TypeSchema::array(TypeSchema::text())),
            ])),
        ),
        StructField::new(
            "progress",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("feature", TypeSchema::text()),
                StructField::new("run", TypeSchema::text()),
                StructField::new("status", TypeSchema::text()),
                StructField::new("summary", TypeSchema::text()),
                StructField::new("check", TypeSchema::text()),
                StructField::new("diffstat", TypeSchema::text()),
            ])),
        ),
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

/// One `_fd_run_traces` row: one attempt at one step, and the context after it
/// (§9, §10.3).
///
/// This is what the run detail draws its timeline from — and what the read-only
/// canvas highlights the path taken with, because the steps named here in `seq`
/// order *are* the path.
fn run_trace_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("seq", TypeSchema::int()),
        StructField::new("step", TypeSchema::text()),
        StructField::new("started_at", TypeSchema::timestamp()),
        StructField::new("finished_at", TypeSchema::timestamp()),
        StructField::new("attempt", TypeSchema::int()),
        // `ok` | `error` | `suspended`.
        StructField::new("outcome", TypeSchema::text()),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        // The context **after** the step, which is what makes a timeline a
        // diff: the change from the row before it is the step's contribution.
        StructField::new("context", TypeSchema::json()),
    ])
}

/// The form a suspended run is waiting for somebody to fill in (§10.3, phase
/// 4.2).
///
/// The fields are the ordinary [`form_field_schema`] vocabulary, so `resumeRun`'s
/// form is rendered by the **same** `SettingsFields` component that renders a
/// trigger's action settings and a file store's — and the step's declaration is
/// carried on the run rather than looked up, so the form somebody is looking at
/// does not change under them when the workflow is edited.
fn pending_form_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("fields", TypeSchema::array(form_field_schema())),
        StructField::new("assign_to", TypeSchema::text()),
        // The role floor for answering; null means admin-only, the same safe
        // reading a trigger's own `min_role` has.
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
    ])
}

/// One version of a workflow, with the program itself and the checks against it
/// (§10.3, phase 5.1).
fn workflow_schema() -> TypeSchema {
    TypeSchema::struct_of([
        // The trigger's id: a workflow is a trigger body, not an entity of its
        // own, so this is the trigger's identity and not a second one.
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        // The table the trigger fires on, which is what decides a step's scope
        // and an action's declaration — the editor needs it to ask `listActions`
        // the right question.
        StructField::new("channel", TypeSchema::optional(TypeSchema::text())),
        StructField::new("version", TypeSchema::int()),
        // The program, in the stored shape (see `saveWorkflow`).
        StructField::new("workflow", TypeSchema::json()),
        // Everything wrong with it, each naming the step it is about so the
        // canvas can mark the node. **Empty is usable**; a workflow with issues
        // is still stored, still listed and still editable — that is what makes
        // it fixable — and only refuses to start a run.
        StructField::new("issues", TypeSchema::array(workflow_issue_schema())),
        StructField::new("versions", TypeSchema::array(workflow_version_schema())),
    ])
}

/// One thing wrong with a workflow, and where it is.
fn workflow_issue_schema() -> TypeSchema {
    TypeSchema::struct_of([
        // Null for a problem about the workflow as a whole rather than any one
        // step (a start step that does not exist, an error policy naming a
        // stranger).
        StructField::new("step", TypeSchema::optional(TypeSchema::text())),
        StructField::new("problem", TypeSchema::text()),
    ])
}

/// One entry in a workflow's history: which version, when, and who saved it.
///
/// The steps are deliberately absent. A history is a list, and carrying every
/// version's whole program would send the same document a dozen times to draw a
/// dropdown; `revertWorkflow` is how an old one is brought back.
fn workflow_version_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("version", TypeSchema::int()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("created_at", TypeSchema::timestamp()),
        StructField::new("created_by", TypeSchema::optional(TypeSchema::uuid())),
    ])
}

/// A registered file-store backend and the settings it declares, so the admin UI
/// can render a form for a backend it knows nothing about. Each setting is a
/// [`form_field_schema`] — the same `FormField` vocabulary a row editor and a
/// framework's settings use.
fn backend_info_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
        StructField::new("operations", TypeSchema::array(operation_schema())),
    ])
}

/// One [`Operation`](sc_types::Operation) a backend declares: an *act* it
/// offers, as opposed to a setting it takes.
///
/// The same "as data" move `form_field_schema` makes, for the other half of
/// what an extension can offer. `scope` says whether it runs against unsaved
/// configuration (`configure`) or a saved store (`instance`), which is what
/// tells the UI where to put the button; `input_spec` is whatever the operation
/// asks the admin for, in the ordinary settings vocabulary, so the same code
/// renders a commit-message box that renders a store's settings.
fn operation_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("scope", TypeSchema::text()),
        StructField::new("input_spec", TypeSchema::array(form_field_schema())),
        StructField::new("on_create", TypeSchema::bool()),
        StructField::new("automatic", TypeSchema::bool()),
    ])
}

/// One entry (file or sub-directory) inside a browsed directory.
fn file_entry_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("path", TypeSchema::text()),
        StructField::new("is_dir", TypeSchema::bool()),
        StructField::new("size", TypeSchema::optional(TypeSchema::int())),
    ])
}

/// One entry as a **listing** reports it: the entry, plus the three facts a file
/// manager draws a column for and would otherwise fetch one request per row.
///
/// It is a wider shape than [`file_entry_schema`] deliberately. `writeFile` and
/// `makeDirectory` answer with the entry they just made, where "who owns it" and
/// "what rule reaches it" are the caller's own answers echoed back; a listing is
/// the one place those are news. `owner` is a **label** — the user's email where
/// the account is still there, and the stored id when it is not — because the
/// column is read by a person, and a UUID in it says nothing.
fn file_listing_entry_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("path", TypeSchema::text()),
        StructField::new("is_dir", TypeSchema::bool()),
        StructField::new("size", TypeSchema::optional(TypeSchema::int())),
        StructField::new("modified", TypeSchema::optional(TypeSchema::text())),
        StructField::new("owner", TypeSchema::optional(TypeSchema::text())),
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
        StructField::new(
            "effective_min_role",
            TypeSchema::optional(TypeSchema::int()),
        ),
    ])
}

/// What a search found: the matching lines, and whether a ceiling cut it short.
///
/// `truncated` is not decoration. A caller that renders results without it tells
/// the reader there is nothing else, which is false exactly when it matters — and
/// it is the difference between "no other uses of this symbol" and "the first
/// hundred uses of this symbol".
fn file_search_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new(
            "matches",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("path", TypeSchema::text()),
                StructField::new("line", TypeSchema::int()),
                StructField::new("column", TypeSchema::int()),
                StructField::new("length", TypeSchema::int()),
                StructField::new("text", TypeSchema::text()),
            ])),
        ),
        StructField::new("files_searched", TypeSchema::int()),
        StructField::new("truncated", TypeSchema::bool()),
    ])
}

/// A file's contents: base64 always, plus decoded `text` when it is valid UTF-8.
fn file_content_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("path", TypeSchema::text()),
        StructField::new("size", TypeSchema::int()),
        StructField::new("base64", TypeSchema::text()),
        StructField::new("text", TypeSchema::optional(TypeSchema::text())),
    ])
}

/// A reference to a UI framework: its registered name and its settings bag. The
/// settings' shape is the framework's own `config_spec`, so `config` is opaque
/// JSON here (the form the SPA renders comes from [`framework_info_schema`]).
fn framework_ref_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("config", TypeSchema::json()),
    ])
}

/// One API provider enabled for an app, on a sub-path.
fn api_config_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("provider", TypeSchema::text()),
        StructField::new("mount", TypeSchema::text()),
        // The provider's own settings, opaque JSON here for the reason a
        // framework's `config` is: what the keys are is the *provider's*
        // declaration (`listApiProviders`' `config_spec`), and the form renders
        // that rather than a shape frozen into this schema.
        StructField::new("config", TypeSchema::json()),
    ])
}

/// A statically-served store subdirectory, on a sub-path.
fn static_dir_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("mount", TypeSchema::text()),
        StructField::new("store", TypeSchema::text()),
        StructField::new("path", TypeSchema::text()),
    ])
}

/// The fields common to an application on the wire — everything but its id. The
/// nested `csp` and `attributes` are opaque JSON (a directive→sources map and a
/// sparse bag respectively), matching how they are stored (§13.2).
fn application_fields() -> Vec<StructField> {
    vec![
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("subdomain", TypeSchema::text()),
        StructField::new("framework", framework_ref_schema()),
        StructField::new(
            "extra_frameworks",
            TypeSchema::array(framework_ref_schema()),
        ),
        StructField::new("tables", TypeSchema::array(TypeSchema::text())),
        StructField::new("file_stores", TypeSchema::array(TypeSchema::text())),
        // The triggers this app exposes as endpoints (§10.2), by name — the same
        // opt-in subset shape the tables and stores have.
        StructField::new("triggers", TypeSchema::array(TypeSchema::text())),
        // …and the streams it exposes for observation (TODO "Streams" §10),
        // the same subset by the same rule: named, or not reachable.
        StructField::new("streams", TypeSchema::array(TypeSchema::text())),
        StructField::new("apis", TypeSchema::array(api_config_schema())),
        StructField::new("static_dirs", TypeSchema::array(static_dir_schema())),
        StructField::new("csp", TypeSchema::json()),
        StructField::new("attributes", TypeSchema::json()),
    ]
}

/// An application as returned by the API: its id, [`application_fields`], and
/// where its source lives.
///
/// `source` is **derived, not stored**: the server resolves the framework's
/// config to a store and a directory (`app_source_from_config`), so the admin UI
/// can link into the file manager at an app's source without knowing how any
/// framework spells that — `code` states it in five settings and `react` derives
/// it from one. `null` for a framework with no source tree.
fn application_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("id", TypeSchema::uuid())];
    fields.extend(application_fields());
    fields.push(StructField::new(
        "source",
        TypeSchema::optional(app_source_schema()),
    ));
    // Whether the framework has a build step. `false` is a framework that is
    // constructed rather than built (Saltcorn UI): saving the application is
    // its deployment, so the list offers no Build button and shows no
    // "not built yet" state.
    fields.push(StructField::new("builds", TypeSchema::bool()));
    // Whether the build installs the project's dependencies itself (a `react`
    // app's `npm install`), so the list offers Deep clean.
    fields.push(StructField::new("installs", TypeSchema::bool()));
    // Whether the application's source is views and pages (Saltcorn UI), so
    // the screen offers the Views and Pages tabs.
    fields.push(StructField::new("has_views", TypeSchema::bool()));
    // The builds its framework offers beside the web bundle — an Android APK —
    // each a button beside Build. Derived from the framework, like `builds`.
    fields.push(StructField::new(
        "targets",
        TypeSchema::array(TypeSchema::struct_of([
            StructField::new("name", TypeSchema::text()),
            StructField::new("label", TypeSchema::text()),
            // Whether this server can build it now — a state, checked each time
            // the list is asked for — and if not, what it lacks, one sentence
            // each ("`ANDROID_HOME` is not set. …").
            StructField::new(
                "readiness",
                TypeSchema::struct_of([
                    StructField::new("ready", TypeSchema::bool()),
                    StructField::new("missing", TypeSchema::array(TypeSchema::text())),
                ]),
            ),
        ])),
    ));
    TypeSchema::Struct(fields)
}

/// A Saltcorn UI view's fields on the wire, less its id. `configuration` is
/// v1's, unchanged; `slug` is v1's `{label, steps}` or absent.
fn view_fields() -> Vec<StructField> {
    vec![
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("viewpattern", TypeSchema::text()),
        StructField::new("table_name", TypeSchema::optional(TypeSchema::text())),
        StructField::new("configuration", TypeSchema::json()),
        StructField::new("min_role", TypeSchema::int()),
        StructField::new("slug", TypeSchema::optional(TypeSchema::json())),
        StructField::new("attributes", TypeSchema::json()),
    ]
}

/// A Saltcorn UI view as returned: its id and [`view_fields`].
fn view_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("id", TypeSchema::uuid())];
    fields.extend(view_fields());
    TypeSchema::Struct(fields)
}

/// The body `saveView` takes: the view less its id, which is the stored view's
/// when the path names one and fresh otherwise.
fn view_input_schema() -> TypeSchema {
    TypeSchema::Struct(view_fields())
}

/// The body `createView` takes: what the admin chooses. The configuration is
/// the pattern's to supply.
fn create_view_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("viewpattern", TypeSchema::text()),
        StructField::new("table_name", TypeSchema::optional(TypeSchema::text())),
        StructField::new("min_role", TypeSchema::int()),
    ])
}

/// The body `viewConfigStep` takes: the pattern and table being configured, the
/// view's name (absent for one not yet saved), the step, counting from 0, and
/// the configuration gathered so far.
fn view_config_step_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("viewpattern", TypeSchema::text()),
        StructField::new("table_name", TypeSchema::optional(TypeSchema::text())),
        StructField::new("name", TypeSchema::optional(TypeSchema::text())),
        StructField::new("step", TypeSchema::int()),
        StructField::new("context", TypeSchema::json()),
    ])
}

/// One step of a view's configuration. `builder` is a layout step, and
/// `builder_options` the options v1's builder is opened with for it (none for a
/// form step); `skip` is a step v1 leaves out for this configuration;
/// `context_field` is the configuration key its values are kept under (none: the
/// top level); `values` are what its form opens with; `issues` are what the form
/// could not express faithfully.
fn view_config_step_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("index", TypeSchema::int()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("count", TypeSchema::int()),
        StructField::new("builder", TypeSchema::bool()),
        StructField::new("builder_options", TypeSchema::optional(TypeSchema::json())),
        StructField::new("skip", TypeSchema::bool()),
        StructField::new("context_field", TypeSchema::optional(TypeSchema::text())),
        StructField::new("blurb", TypeSchema::optional(TypeSchema::text())),
        StructField::new("fields", TypeSchema::array(form_field_schema())),
        StructField::new("values", TypeSchema::json()),
        StructField::new("issues", TypeSchema::array(TypeSchema::text())),
    ])
}

/// What refers to a view by name: the views embedding it, the views linking to
/// it, the pages and library items showing it — and the library items it places.
fn view_references_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("embedded_in", TypeSchema::array(TypeSchema::text())),
        StructField::new("linked_from", TypeSchema::array(TypeSchema::text())),
        StructField::new("pages", TypeSchema::array(TypeSchema::text())),
        // The library items whose layouts show or link to it (TODO "The
        // builder" §8).
        StructField::new("library", TypeSchema::array(TypeSchema::text())),
        // The other direction: the library items its own layout places.
        StructField::new("places", TypeSchema::array(TypeSchema::text())),
    ])
}

/// A Saltcorn UI page's fields on the wire, less its id. `attributes` carries
/// `root_page_for_roles`.
fn page_fields() -> Vec<StructField> {
    vec![
        StructField::new("name", TypeSchema::text()),
        StructField::new("title", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("layout", TypeSchema::json()),
        StructField::new("min_role", TypeSchema::int()),
        StructField::new("attributes", TypeSchema::json()),
    ]
}

/// A Saltcorn UI page as returned: its id and [`page_fields`].
fn page_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("id", TypeSchema::uuid())];
    fields.extend(page_fields());
    TypeSchema::Struct(fields)
}

/// The body `savePage` takes.
fn page_input_schema() -> TypeSchema {
    TypeSchema::Struct(page_fields())
}

/// One of v1's in-place edits to a placed library item: the item, and its whole
/// new layout.
fn library_update_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("library_id", TypeSchema::uuid()),
        StructField::new("layout", TypeSchema::json()),
    ])
}

/// The body `saveViewLayout` takes: the builder step, counting from 0, what the
/// builder wrote for it (v1's `columns` and `layout`), and its edits to the
/// library items the layout places.
fn view_layout_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("step", TypeSchema::int()),
        StructField::new("columns", TypeSchema::json()),
        StructField::new("layout", TypeSchema::json()),
        StructField::new(
            "libraryUpdates",
            TypeSchema::optional(TypeSchema::array(library_update_schema())),
        ),
    ])
}

/// The body `savePageLayout` takes: the page's new layout and the library edits
/// it carries.
fn page_layout_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("layout", TypeSchema::json()),
        StructField::new(
            "libraryUpdates",
            TypeSchema::optional(TypeSchema::array(library_update_schema())),
        ),
    ])
}

/// What names a page: the menu entries' labels, the roles (by name) whose home
/// page it is, and the views, pages and library items showing or linking to it —
/// and the library items its own layout places.
fn page_references_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("menu", TypeSchema::array(TypeSchema::text())),
        StructField::new("home_page_for", TypeSchema::array(TypeSchema::text())),
        StructField::new("views", TypeSchema::array(TypeSchema::text())),
        StructField::new("pages", TypeSchema::array(TypeSchema::text())),
        StructField::new("library", TypeSchema::array(TypeSchema::text())),
        StructField::new("places", TypeSchema::array(TypeSchema::text())),
    ])
}

/// What places a library item, by name: views, pages and other items, each
/// directly or through an item they place.
fn library_references_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("views", TypeSchema::array(TypeSchema::text())),
        StructField::new("pages", TypeSchema::array(TypeSchema::text())),
        StructField::new("library", TypeSchema::array(TypeSchema::text())),
    ])
}

/// A library item as returned: v1's `{ name, icon, layout }`, with this server's
/// UUID id, description and attributes. The layout is v1's, untouched.
fn library_item_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("icon", TypeSchema::text()),
        StructField::new("layout", TypeSchema::json()),
        StructField::new("attributes", TypeSchema::json()),
    ])
}

/// A library item as `listLibrary` returns it: [`library_item_schema`] and what
/// places it.
fn listed_library_item_schema() -> TypeSchema {
    let TypeSchema::Struct(mut fields) = library_item_schema() else {
        unreachable!("library_item_schema is a struct")
    };
    fields.push(StructField::new("used_by", library_references_schema()));
    TypeSchema::Struct(fields)
}

/// The body `createLibraryItem` takes: v1's `savefrombuilder` body, and a
/// description.
fn create_library_item_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("icon", TypeSchema::optional(TypeSchema::text())),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("layout", TypeSchema::json()),
    ])
}

/// The body `saveLibraryItem` takes: the name, and the icon and description when
/// they change.
fn save_library_item_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("icon", TypeSchema::optional(TypeSchema::text())),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
    ])
}

/// The body `saveLibraryUpdates` takes: v1's `save-updates` body.
fn library_updates_input_schema() -> TypeSchema {
    TypeSchema::struct_of([StructField::new(
        "libraryUpdates",
        TypeSchema::array(library_update_schema()),
    )])
}

/// A preview, as the HTML v1's route sends for the canvas.
fn builder_html_schema() -> TypeSchema {
    TypeSchema::struct_of([StructField::new("html", TypeSchema::text())])
}

/// The body `builderFieldPreview` takes: v1's path (table, field, fieldview)
/// and v1's body (the fieldview's configuration, and the row a Show previews).
fn builder_field_preview_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("table", TypeSchema::text()),
        StructField::new("field", TypeSchema::text()),
        StructField::new("fieldview", TypeSchema::text()),
        StructField::new("configuration", TypeSchema::optional(TypeSchema::json())),
        StructField::new("row_id", TypeSchema::optional(TypeSchema::json())),
    ])
}

/// The body `builderFieldviewConfigForm` takes: v1's path (the table) and v1's
/// body, whose members say which kind of column asks — a field, a join field or
/// an aggregation.
fn builder_fieldview_config_input_schema() -> TypeSchema {
    let optional = |name: &str| StructField::new(name, TypeSchema::optional(TypeSchema::text()));
    TypeSchema::struct_of([
        StructField::new("table", TypeSchema::text()),
        optional("field_name"),
        optional("fieldview"),
        optional("type"),
        optional("join_field"),
        optional("join_fieldview"),
        optional("agg_outcome_type"),
        optional("agg_fieldview"),
        optional("agg_field"),
        optional("mode"),
        optional("_columndef"),
    ])
}

/// A registered view pattern: the name a view's `viewpattern` holds, how the
/// picker presents it, and what the runtime's manifest says about it. `label`,
/// `description`, `view_quantity`, `routes` and `steps` come from the running
/// view runtime; with none running they are the name and empty.
fn view_pattern_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("table_required", TypeSchema::bool()),
        StructField::new("view_quantity", TypeSchema::optional(TypeSchema::text())),
        StructField::new("routes", TypeSchema::array(TypeSchema::text())),
        StructField::new("steps", TypeSchema::array(TypeSchema::text())),
        // The module that declared it; absent for a built-in.
        StructField::new("module", TypeSchema::optional(TypeSchema::text())),
    ])
}

/// Where an application's source lives: a file store and a directory in it.
fn app_source_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("store", TypeSchema::text()),
        StructField::new("path", TypeSchema::text()),
    ])
}

/// A freshly created application, plus what scaffolding it did (§2.3) and which
/// agent was created to build it (§13.3).
///
/// Only `create` carries these: a `react` app's project is generated on its first
/// save, and the admin should see that it happened — or why it did not — without
/// a second request. `scaffolded` is a summary line; `scaffold_error` explains a
/// scaffold that was refused (an occupied directory, an unreachable store) on an
/// application that was nonetheless created, since the row is valid either way.
/// `agent` and `agent_error` report the builder agent the same way: its name, or
/// why the deployment could not create one (no LLM provider connected). `building`
/// says the server started the application's first build itself, so its subdomain
/// will serve without a restart and without the Build button.
fn created_application_schema() -> TypeSchema {
    let TypeSchema::Struct(mut fields) = application_schema() else {
        unreachable!("application_schema is a struct")
    };
    fields.push(StructField::new(
        "scaffolded",
        TypeSchema::optional(TypeSchema::text()),
    ));
    fields.push(StructField::new(
        "scaffold_error",
        TypeSchema::optional(TypeSchema::text()),
    ));
    // The agent that builds this application, which its framework declares
    // (§13.3): its name when one was created, or why one was not — the same
    // alongside-not-instead-of reporting the scaffold gets, and for the same
    // reason. The application is created either way.
    fields.push(StructField::new(
        "agent",
        TypeSchema::optional(TypeSchema::text()),
    ));
    fields.push(StructField::new(
        "agent_error",
        TypeSchema::optional(TypeSchema::text()),
    ));
    // Whether the server started the application's **first build** as part of
    // creating it (§13.2). Creating an application is what deploys it — the boot
    // path built every stored application, so anything else made a restart part
    // of creating one — but a first build is `npm install` plus a bundler, which
    // is not a thing to hold this response open for. So it runs in the background
    // and this says to expect the subdomain to start serving shortly. Absent for
    // an application with nothing to build.
    fields.push(StructField::new(
        "building",
        TypeSchema::optional(TypeSchema::bool()),
    ));
    // The local file stores created for this application because a store
    // setting asked for a new one (`sc_catalog::NEW_LOCAL_FILE_STORE`) — named
    // after the subdomain, in the directory "Suggest a directory" would have
    // picked. Absent when nothing was created.
    fields.push(StructField::new(
        "created_file_stores",
        TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
    ));
    TypeSchema::Struct(fields)
}

/// The body accepted when creating or updating an application: the same fields
/// minus the id (server-assigned on create, taken from the path on update).
fn application_input_schema() -> TypeSchema {
    TypeSchema::Struct(application_fields())
}

/// One build of a target, as the admin UI polls it: where it has got to, where
/// its log is from the moment it starts, and — once finished — the artifact or
/// the error.
fn target_build_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("target", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        // `running`, `succeeded` or `failed`.
        StructField::new("status", TypeSchema::text()),
        StructField::new("store", TypeSchema::text()),
        StructField::new("log_path", TypeSchema::text()),
        StructField::new("started_at", TypeSchema::timestamp()),
        StructField::new("finished_at", TypeSchema::optional(TypeSchema::timestamp())),
        StructField::new("artifact", TypeSchema::optional(TypeSchema::text())),
        StructField::new("size", TypeSchema::optional(TypeSchema::int())),
        // The end of the log, once finished; the whole of it is at `log_path`.
        StructField::new("log", TypeSchema::optional(TypeSchema::text())),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
    ])
}

/// The outcome of a build: whether it built, whether its source is a git repo,
/// and the bundler's log. (A *failed* build is not this shape — it is an
/// Application error whose message carries the diagnostics, §16.)
fn build_result_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("built", TypeSchema::bool()),
        StructField::new("git_repo", TypeSchema::bool()),
        StructField::new("log", TypeSchema::text()),
    ])
}

/// One stored trigger: what fires it, what it runs, and whether it is usable.
///
/// `error` is the part that makes a broken trigger fixable rather than merely
/// absent, exactly as a file store's is: a trigger whose table was dropped or
/// whose action a removed plugin provided is **not in the live set** and will not
/// fire, but it is still stored, still listed and still editable — and the reason
/// is the only thing that says what to fix.
fn trigger_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("id", TypeSchema::uuid())];
    fields.extend(trigger_fields());
    fields.push(StructField::new(
        "error",
        TypeSchema::optional(TypeSchema::text()),
    ));
    // Read-only, and absent from the input shape below: when a periodic trigger
    // last fired is the scheduler's record of what happened, not a field an
    // admin sets. The list shows it; nothing posts it back.
    fields.push(StructField::new(
        "last_run_at",
        TypeSchema::optional(TypeSchema::timestamp()),
    ));
    // The workflow half, and read-only for the same reason `last_run_at` is: a
    // workflow's steps are versions of their own, written through the workflow
    // endpoints. Null on an action body. The list shows them because "a
    // workflow" is not a useful description of a trigger — how many steps it has
    // and which version is live is what tells one apart from another, and the
    // alternative was one `getWorkflow` per row.
    fields.push(StructField::new(
        "workflow_version",
        TypeSchema::optional(TypeSchema::int()),
    ));
    fields.push(StructField::new(
        "workflow_steps",
        TypeSchema::optional(TypeSchema::int()),
    ));
    TypeSchema::Struct(fields)
}

/// The body accepted when creating or updating a trigger: the same fields minus
/// the id (server-assigned on create, taken from the path on update) and minus
/// `error` (which is the server's answer, not the admin's input).
fn trigger_input_schema() -> TypeSchema {
    TypeSchema::Struct(trigger_fields())
}

/// The editable half of a trigger, shared by the read and write shapes.
fn trigger_fields() -> Vec<StructField> {
    vec![
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // The event kind, as the lowercase word it is stored under
        // (`insert`, `login`, …).
        StructField::new("when", TypeSchema::text()),
        // The table, for a table event; null for every other kind.
        StructField::new("channel", TypeSchema::optional(TypeSchema::text())),
        StructField::new("only_if", TypeSchema::optional(TypeSchema::text())),
        // Which engine runs it: `action` or `workflow` (§10.3). Absent on input
        // means `action`, which is what every trigger was before workflows and
        // what a client that has not heard of them sends.
        StructField::new("body", TypeSchema::optional(TypeSchema::text())),
        // Both are the **action** body's, and both are null for a workflow —
        // whose steps are a version of their own, read and written through the
        // workflow endpoints rather than as a field of the trigger.
        StructField::new("action", TypeSchema::optional(TypeSchema::text())),
        StructField::new("configuration", TypeSchema::optional(TypeSchema::json())),
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
        StructField::new("enabled", TypeSchema::bool()),
        // The periodic timing (§10.2), null on the kinds that have none. Three
        // flat fields rather than a nested object: each is one number, each is
        // one input in the form, and a kind that does not use one refuses it —
        // so a nested shape would only add a level to say the same thing.
        StructField::new("minute", TypeSchema::optional(TypeSchema::int())),
        StructField::new("hour", TypeSchema::optional(TypeSchema::int())),
        StructField::new("day_of_week", TypeSchema::optional(TypeSchema::int())),
    ]
}

/// A registered action and the settings it declares — name, one-line
/// description, and its `config_spec` in the same [`form_field_schema`]
/// vocabulary a framework's and a file-store backend's settings use.
fn action_info_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
        // Whether this action can be offered as a **workflow step** on the table
        // asked about (§10.3, phase 5.4): false when its declaration for that
        // channel has a required setting with nothing to pick, which is what an
        // action that cannot serve this channel looks like from the outside. The
        // step palette hides those rather than offering a step whose settings
        // form cannot be completed.
        //
        // Decided from the declaration, not from a list of action names, so a
        // plugin's action is judged by the same rule as a built-in.
        StructField::new("workflow_step", TypeSchema::bool()),
    ])
}

/// A registered framework and the settings it declares, so the admin UI can
/// render a form for a framework it knows nothing about (§13.3). Each setting is
/// a [`form_field_schema`] — the same `FormField` vocabulary a row editor uses.
fn framework_info_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        // The editorial half: a human name and a sentence saying who each
        // framework is for. The registry owns it, so the picker can present two
        // frameworks as the different propositions they are (§2.4) while staying
        // free of any knowledge of a particular one.
        StructField::new("label", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
        // Whether an application on this framework has views and pages
        // (Saltcorn UI), like the application row's own `has_views`. Its
        // settings are then the running application's — the menu, the login
        // form, the languages — so the screen keeps them off the create form
        // and edits them on the application's App settings tab.
        StructField::new("has_views", TypeSchema::bool()),
        // The settings whose value names a file store. `config_spec` cannot say
        // so — by the time it is sent, a store picker has been resolved to a
        // plain list of names — and a new application's form needs to know,
        // because on those pickers it offers to create a local store rather
        // than making the admin leave the form to define one first.
        StructField::new("file_store_settings", TypeSchema::array(TypeSchema::text())),
        // The settings whose value is a file in the application's store (an
        // app icon, a keystore), each with the extensions it accepts: the form
        // fills their choices from `listStoreFiles` for the store the
        // application names.
        StructField::new(
            "file_settings",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("extensions", TypeSchema::array(TypeSchema::text())),
            ])),
        ),
        // The builds the framework offers beside its web bundle, each with the
        // settings that configure it alone. Those settings are in `config_spec`
        // too; the form shows them under their target instead of among the
        // framework's own.
        StructField::new(
            "targets",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("label", TypeSchema::text()),
                StructField::new("options", TypeSchema::array(TypeSchema::text())),
                // What the module does for the target on request: a button
                // each, shown while its `show_if` holds.
                StructField::new(
                    "operations",
                    TypeSchema::array(TypeSchema::struct_of([
                        StructField::new("name", TypeSchema::text()),
                        StructField::new("label", TypeSchema::text()),
                        StructField::new("description", TypeSchema::text()),
                        StructField::new(
                            "show_if",
                            TypeSchema::array(TypeSchema::struct_of([
                                StructField::new("name", TypeSchema::text()),
                                StructField::new("values", TypeSchema::array(TypeSchema::json())),
                            ])),
                        ),
                    ])),
                ),
            ])),
        ),
    ])
}

/// A registered API provider as the application form needs it: the name it is
/// stored under, how to present it, the sub-path it is usually mounted at, and
/// the settings it takes.
///
/// The `config_spec` is the same [`form_field_schema`] a framework's is, and it
/// is here for the same reason (§13.3): the form renders whatever the provider
/// declares, so GraphQL's aggregation switch is a control on a screen that knows
/// nothing about GraphQL, and a provider that grows a setting grows a control
/// without the admin UI being touched.
fn api_provider_info_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // What the form fills the mount box in with when this provider is
        // picked, so the common case is no typing at all.
        StructField::new("default_mount", TypeSchema::text()),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
        // Whether this provider serves custom SQL queries, i.e. whether the
        // application form offers the query editor for it. Declared by the
        // provider for the same reason its settings are: the form renders what
        // it is told rather than checking for a provider by name.
        StructField::new("supports_custom_queries", TypeSchema::bool()),
    ])
}

/// The body `describeCustomQuery` takes: one custom query as the editor holds
/// it, plus the tables the application it belongs to declares. `language` is
/// `sql` when absent; `code` is the source in whichever language it names.
///
/// It is the stored [`CustomQuery`](crate::CustomQuery) shape rather than "just
/// the SQL and the parameters" so that the *whole* refusal an eventual save
/// would give arrives from the check button: a name a table endpoint already
/// holds and a path a table's own routes already answer are both about this
/// query, and learning about them at save time — after the SQL has been declared
/// fine — is two round trips to fix one query. `tables` is the application's
/// declared subset; an application not yet created sends the ones typed into the
/// form, and an empty list simply skips those two rules.
fn custom_query_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("method", TypeSchema::text()),
        StructField::new("path", TypeSchema::text()),
        StructField::new("language", TypeSchema::optional(TypeSchema::text())),
        StructField::new("code", TypeSchema::text()),
        StructField::new(
            "params",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("type", TypeSchema::text()),
                StructField::new("required", TypeSchema::optional(TypeSchema::bool())),
            ])),
        ),
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
        StructField::new(
            "tables",
            TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
        ),
    ])
}

/// One column of a custom query's result, as the database described it: the name
/// it arrives under in the JSON, and the wire type it was mapped to.
fn query_column_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("type", TypeSchema::text()),
    ])
}

/// The whole settings screen in one response: what may be set, and what is set.
///
/// `values` is opaque JSON — a bag keyed by the declared settings' names — for
/// the same reason a file store's `config` is: its shape is the declarations',
/// which are data, and a static type could only describe it by freezing it.
/// Secrets in it are the redaction sentinel, never the stored value.
fn settings_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("sections", TypeSchema::array(settings_section_schema())),
        StructField::new("values", TypeSchema::json()),
        // The keys pinned by this host's `feldspar.toml`, which win over the
        // stored values and which a save cannot change.
        StructField::new("host_keys", TypeSchema::array(TypeSchema::text())),
    ])
}

/// One group of settings: its heading, what it is for, and its keys.
fn settings_section_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("fields", TypeSchema::array(settings_field_schema())),
    ])
}

/// A settings key: the same declaration every other configurable thing carries,
/// plus the sentence a settings screen has room to put under the control.
fn settings_field_schema() -> TypeSchema {
    let TypeSchema::Struct(fields) = form_field_schema() else {
        // `form_field_schema` is a struct literal one function away; this arm
        // exists because the type says it might not be, not because it can.
        return form_field_schema();
    };
    TypeSchema::struct_of(
        fields
            .into_iter()
            .chain([StructField::new("help", TypeSchema::text())]),
    )
}

/// One settings field of a framework's `config_spec`: enough for the admin UI to
/// render and label an input control for it.
/// One model provider the picker offers (TODO §10).
///
/// `config_spec` and `hyperparameters` are the ordinary
/// [`form_field_schema`] vocabulary — the same one a file-store backend, an
/// agent trait and an action declare their settings in — so the model form
/// renders a provider it has never heard of, whether it is a built-in or one a
/// module supplied.
///
/// `outcome` is what a fit of *this configuration* would produce, and it is
/// present only when the request carried a dataset and a configuration that
/// resolve to one; `outcome_error` is the sentence saying why it does not,
/// which is usually "no column is named as the label" and is what the form is
/// waiting to be told.
fn model_provider_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // The module supplying it, or null for a built-in — what the picker
        // renders "built in" against.
        StructField::new("module", TypeSchema::optional(TypeSchema::text())),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
        StructField::new("hyperparameters", TypeSchema::array(form_field_schema())),
        // The declaration: which configuration key holds the label, and what
        // happens to it. What the form switches on before a dataset exists.
        StructField::new("outcome_spec", TypeSchema::json()),
        StructField::new("outcome", TypeSchema::optional(TypeSchema::json())),
        StructField::new("outcome_error", TypeSchema::optional(TypeSchema::text())),
        // Whether the host standardises the numeric features before handing
        // them over — a k-means says yes, a regression says no because a
        // coefficient in the data's own units is what somebody reads it for.
        StructField::new("standardise", TypeSchema::bool()),
        // Whether it takes a program's data bound from the datasets — what the
        // form renders the binding editor for (Stan TODO §18).
        StructField::new("binds_data", TypeSchema::bool()),
        // Whether a running fit can be cancelled — what the instance screen
        // renders Cancel for.
        StructField::new("cancellable", TypeSchema::bool()),
        // Why it cannot fit anything on this server, when it cannot: "CmdStan
        // was not found: …". Listed rather than hidden (Stan TODO §4).
        StructField::new("unavailable", TypeSchema::optional(TypeSchema::text())),
    ])
}

/// One column of a previewed dataset: its name and the type its values came
/// back as.
///
/// The type is the **data's**, not the schema's, and that is not a shortcut: a
/// `SchemaShape` carries no types at all, and the type of `price / area`, of a
/// join path or of an aggregation is not derivable from one.
fn dataset_column_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("type", TypeSchema::text()),
    ])
}

/// One stored model, as the list and the form see it (TODO §15).
///
/// `error` is the twin of an agent's and a trigger's: a model that stopped
/// validating — a dataset column whose formula no longer resolves, a provider
/// whose module was uninstalled — is **still listed and still editable**,
/// because editing it is the repair.
///
/// `last_fit` rides along because that is what the list is read for: which
/// models have been fitted, when, and whether the last one worked.
fn model_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("provider", TypeSchema::text()),
        StructField::new("table_name", TypeSchema::text()),
        StructField::new("dataset", TypeSchema::json()),
        // The datasets beside the main one, each under the name bindings
        // address it by (Stan TODO §7); `[]` for every model that has none.
        StructField::new("related", TypeSchema::json()),
        StructField::new("configuration", TypeSchema::json()),
        StructField::new("hyperparameters", TypeSchema::json()),
        StructField::new("split", TypeSchema::json()),
        StructField::new("attributes", TypeSchema::json()),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        StructField::new("instances", TypeSchema::int()),
        StructField::new("last_fit", TypeSchema::optional(model_instance_schema())),
        StructField::new(
            "active_instance",
            TypeSchema::optional(model_instance_schema()),
        ),
        // On `saveModel`'s answer for a program provider: `stanc`'s warnings,
        // or the notice that the program was not checked (Stan TODO §5). Null
        // everywhere else.
        StructField::new("program_check", TypeSchema::optional(TypeSchema::json())),
        // The screens' layout (analytics TODO A3.4), written only by
        // `patchModelViewState`; `{}` for a new model.
        StructField::new("view_state", TypeSchema::optional(TypeSchema::json())),
    ])
}

/// What a `saveModel` sends. The id is what says create or replace.
fn model_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("provider", TypeSchema::text()),
        StructField::new("dataset", TypeSchema::json()),
        StructField::new("related", TypeSchema::optional(TypeSchema::json())),
        StructField::new("configuration", TypeSchema::optional(TypeSchema::json())),
        // Per hyperparameter either a value or a **list** of values, and a fit
        // runs the grid of the lists (§11). One field rather than two, because a
        // list of one and a scalar are the same search.
        StructField::new("hyperparameters", TypeSchema::optional(TypeSchema::json())),
        StructField::new("split", TypeSchema::optional(TypeSchema::json())),
        StructField::new("attributes", TypeSchema::optional(TypeSchema::json())),
    ])
}

/// One fit, as a list sees it: everything except the parameters, the metrics and
/// the encoding, which a list of forty fits must not carry.
fn model_instance_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("model", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // `fitting` | `fitted` | `failed`. A column rather than an attribute
        // because every row has one and it is what the list filters on; the
        // failure **sentence** is the other way round, present only on the rows
        // that failed, which is why it is `error` here and an attribute in the
        // row (§15).
        StructField::new("status", TypeSchema::text()),
        StructField::new("created", TypeSchema::timestamp()),
        StructField::new("active", TypeSchema::bool()),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        StructField::new("hyperparameters", TypeSchema::json()),
        StructField::new("outcome", TypeSchema::optional(TypeSchema::json())),
        StructField::new("metrics", TypeSchema::json()),
        StructField::new("rows", TypeSchema::optional(TypeSchema::json())),
        // A running posterior fit's stage and per-chain iterations, written at
        // most once a second (Stan TODO §13); null otherwise.
        StructField::new("progress", TypeSchema::optional(TypeSchema::json())),
        // Whether somebody has asked this fit to stop.
        StructField::new("cancel_requested", TypeSchema::bool()),
        // A posterior's diagnostic warnings, as sentences that say what to do
        // (§15); empty for every other fit.
        StructField::new("warnings", TypeSchema::array(TypeSchema::text())),
        // Whether the model's datasets differ now from the ones this fit read
        // (analytics TODO A1.10, A3.3): on every fit a list or a model shows,
        // so the model editor's list of fits can say which are out of date;
        // null when it cannot tell, or where it was not asked.
        StructField::new("dataset_changed", TypeSchema::optional(TypeSchema::bool())),
    ])
}

/// One fit in full, which is the instance screen: the list's fields plus the
/// parameter blocks, the encoding and every grid point that was tried.
///
/// The state is deliberately **not** here. It is the provider's serialised fit,
/// opaque to everything but the provider, and it is the big column: a random
/// forest's is every tree.
fn model_instance_detail_schema() -> TypeSchema {
    let TypeSchema::Struct(summary) = model_instance_schema() else {
        // `model_instance_schema` is a struct literal above; this arm cannot be
        // reached and returning the summary is the harmless reading if it were.
        return model_instance_schema();
    };
    TypeSchema::Struct(
        summary
            .into_iter()
            .chain([
                // Scalar, table or text — three renderings, and the admin UI
                // never has to know what a coefficient, a cluster centre or an
                // explained-variance ratio is (§7).
                StructField::new("parameters", TypeSchema::array(TypeSchema::json())),
                StructField::new("encoding", TypeSchema::json()),
                // Every hyperparameter point tried and what it scored, so the
                // search is inspectable and not a number that appeared (§11).
                StructField::new("search", TypeSchema::array(TypeSchema::json())),
                // A posterior's output variables: each one's shape and the
                // dimension labelling each axis (Stan TODO §16) — what the
                // draws and the summary endpoints are asked about.
                StructField::new("variables", TypeSchema::json()),
                // A posterior's binding report: rows read and bound, drops, a
                // line per data variable.
                StructField::new("binding", TypeSchema::optional(TypeSchema::json())),
                // Whether the model's program differs now from the one this
                // fit snapshotted (Stan TODO §§6, 18); null for a provider with
                // no program, or when it cannot tell.
                StructField::new("program_changed", TypeSchema::optional(TypeSchema::bool())),
            ])
            .collect(),
    )
}

/// One prediction, and which row it is for.
///
/// `value` is what the prediction **writes into a row** (§12) — the number, the
/// class name, the cluster index or the vector — beside the structured
/// `prediction` the screen renders, which also carries the probability where the
/// provider gave one.
fn prediction_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("prediction", TypeSchema::json()),
        StructField::new("value", TypeSchema::json()),
        // The row's primary key, for a prediction over the dataset; null for a
        // literal row, which has none.
        StructField::new("key", TypeSchema::optional(TypeSchema::text())),
    ])
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
        // The floor for **observing** it through an application. Null is
        // admin-only — the trigger rule, for the trigger reason: a flow nobody
        // has thought about the access of is not public.
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
        StructField::new("attributes", TypeSchema::json()),
        // Lifted out of `attributes` for the list's switch, as a trigger's is:
        // it is the one attribute every row has an answer for.
        StructField::new("enabled", TypeSchema::bool()),
        StructField::new("element_type", TypeSchema::optional(TypeSchema::json())),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        StructField::new("status", TypeSchema::optional(TypeSchema::json())),
        StructField::new("counters", TypeSchema::optional(TypeSchema::json())),
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
    ])
}

fn form_field_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("type", TypeSchema::text()),
        StructField::new("required", TypeSchema::bool()),
        StructField::new("default", TypeSchema::optional(TypeSchema::json())),
        StructField::new("options", TypeSchema::array(TypeSchema::json())),
        StructField::new("multiline", TypeSchema::bool()),
        // Whether the value is a secret (§11.1): the form renders a password
        // input, and what it is handed for this field is the redaction
        // sentinel, never the stored key.
        StructField::new("secret", TypeSchema::bool()),
        // Whether the value is fixed once the thing exists: the form renders the
        // control read-only on an edit, and a save that changes it anyway is
        // overwritten with what is stored.
        StructField::new("create_only", TypeSchema::bool()),
        // The language this value is source code in (`"javascript"`), or null for
        // a setting that is not code: the form renders a code editor for it.
        StructField::new("code_language", TypeSchema::optional(TypeSchema::text())),
        // When the setting applies: every named setting holds one of its
        // values. Empty means always. The form hides a setting that does not
        // apply, and the server does not require it.
        StructField::new(
            "show_if",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("values", TypeSchema::array(TypeSchema::json())),
            ])),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{Area, Projection};

    /// The census (§13.6). A tag is a decision about somebody's context window,
    /// so adding one has to mean editing this list — which is the moment to
    /// write down why the tool earns its place.
    #[test]
    fn the_tier_two_tags_are_the_ones_that_were_argued_for() {
        let set = admin_endpoints();
        let tagged: Vec<&str> = set
            .iter()
            .filter(|e| e.mcp.is_some())
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(
            tagged,
            [
                "listTableProviders",
                "listFieldTypes",
                "listAgents",
                "createAgent",
                "updateAgent",
                "deleteAgent",
                "listAgentTraits",
                "listRuns",
                "getRun",
                "listApplications",
                "buildApplication",
                "listTriggers",
                "listActions",
                "getWorkflow",
                "saveWorkflow",
                "revertWorkflow",
                "listWorkflowRuns",
            ]
        );
    }

    /// A build is the one kind of failure that is *news about the thing* rather
    /// than a refusal of the call, so exactly one endpoint says so. A second one
    /// would be a second endpoint whose errors stop looking like errors, and that
    /// is a decision to argue for here rather than to acquire.
    #[test]
    fn only_the_build_reports_its_failure_as_a_result() {
        let set = admin_endpoints();
        let builds: Vec<&str> = set
            .iter()
            .filter(|e| e.mcp.as_ref().is_some_and(|tag| tag.build_result))
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(builds, ["buildApplication"]);
    }

    /// Tier 3 is a list of things that are *absent*, which no compiler checks.
    /// These four are the ones §13.6 names, and each would be a different
    /// proposition from administering an application.
    #[test]
    fn the_surfaces_that_were_left_out_stayed_out() {
        let set = admin_endpoints();
        for name in [
            // Row data: an agent that can add a column and one that can read
            // customer rows are not the same offer.
            "listRows",
            // The file-store IDE: the coding agent has the repository already.
            "readFile",
            "writeFile",
            // Backup, restore and user management.
            "restoreBackup",
            "clearAll",
            "listUsers",
            "createUser",
            // And above all: a token that could mint tokens could not be
            // revoked (phase 2.4 says so in its own comment).
            "createApiToken",
            "listApiTokens",
            "revokeApiToken",
        ] {
            let endpoint = set.find(name).expect("endpoint should exist");
            assert!(
                endpoint.mcp.is_none(),
                "`{name}` is tier 3 and must not be projected as an MCP tool"
            );
        }
    }

    /// An area that is off must mean the same thing whichever tier the tool came
    /// from, so a tagged trigger or application endpoint declares its half.
    #[test]
    fn a_tagged_endpoint_declares_the_half_of_the_surface_it_belongs_to() {
        let set = admin_endpoints();
        let area_of = |name: &str| {
            set.find(name)
                .and_then(|e| e.mcp.as_ref())
                .and_then(|tag| tag.area)
        };
        assert_eq!(area_of("listTriggers"), Some(Area::Triggers));
        assert_eq!(area_of("listActions"), Some(Area::Triggers));
        // A workflow is a trigger's body (§10.3), so it is the triggers half.
        assert_eq!(area_of("saveWorkflow"), Some(Area::Triggers));
        assert_eq!(area_of("listWorkflowRuns"), Some(Area::Triggers));
        assert_eq!(area_of("buildApplication"), Some(Area::Applications));
        assert_eq!(area_of("listApplications"), Some(Area::Applications));
        // The schema half has no area: it is what this surface is.
        assert_eq!(area_of("listFieldTypes"), None);
        assert_eq!(area_of("listAgents"), None);
    }

    /// The merge of path, query and body into one arguments object is only
    /// unambiguous while the three name different things.
    #[test]
    fn no_tagged_endpoint_merges_two_arguments_under_one_name() {
        for projection in Projection::all(&admin_endpoints()) {
            let names = projection.argument_names();
            let mut seen = std::collections::HashSet::new();
            for name in &names {
                assert!(
                    seen.insert(name.clone()),
                    "`{}` declares `{name}` twice across its path, query and body",
                    projection.name()
                );
            }
            // And every one of them is an object schema, which is what an MCP
            // client requires of an `inputSchema`.
            assert_eq!(
                projection.parameters()["type"],
                serde_json::json!("object"),
                "{}'s parameters",
                projection.name()
            );
        }
    }

    /// A tool that changes something declares the grant that allows it, so the
    /// six flags mean one thing across both tiers. A read declares none — every
    /// one of these is `admin()` already, and the grants are about changes.
    #[test]
    fn a_tagged_endpoint_that_changes_something_declares_the_grant_for_it() {
        let set = admin_endpoints();
        let grant_of = |name: &str| {
            set.find(name)
                .and_then(|e| e.mcp.as_ref())
                .and_then(|tag| tag.grant)
        };
        assert_eq!(grant_of("createAgent"), Some(Grant::Create));
        assert_eq!(grant_of("updateAgent"), Some(Grant::Edit));
        assert_eq!(grant_of("deleteAgent"), Some(Grant::Drop));
        assert_eq!(grant_of("saveWorkflow"), Some(Grant::Edit));
        assert_eq!(grant_of("revertWorkflow"), Some(Grant::Edit));
        // Building writes the generated client and replaces what a subdomain
        // serves: a change to what is there, whatever the method reads as.
        assert_eq!(grant_of("buildApplication"), Some(Grant::Edit));

        for read_only in [
            "listAgents",
            "listAgentTraits",
            "listRuns",
            "getRun",
            "listApplications",
            "listTriggers",
            "listActions",
            "listFieldTypes",
            "listTableProviders",
            "getWorkflow",
            "listWorkflowRuns",
        ] {
            assert_eq!(grant_of(read_only), None, "`{read_only}` only reads");
        }

        // And nothing tagged is a write with no grant behind it: a method that
        // is not GET has to have said which flag allows it.
        for endpoint in set.iter().filter(|e| e.mcp.is_some()) {
            if endpoint.method != Method::Get {
                assert!(
                    endpoint.mcp.as_ref().and_then(|t| t.grant).is_some(),
                    "`{}` changes something and declares no grant",
                    endpoint.name
                );
            }
        }
    }

    /// §13.6's load-bearing rule, asserted rather than trusted: every tool this
    /// server offers is behind the admin requirement it was already behind.
    #[test]
    fn every_projected_endpoint_is_still_admin_only() {
        for projection in Projection::all(&admin_endpoints()) {
            assert_eq!(
                projection.auth(),
                &AuthRequirement::admin(),
                "`{}` is projected as an MCP tool and must be admin-only",
                projection.name()
            );
        }
    }
}
