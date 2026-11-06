//! The schema half of the administrative surface (§13.6): `describe_schema`
//! and `edit_schema`.
//!
//! What is here is the **wire shape** — the tools' descriptions, their JSON
//! schemas, and the parse from one wire item to an
//! [`Operation`](crate::schema_edit::Operation). The transaction, the grant
//! refusals and the DDL are [`crate::schema_edit`]'s, so the admin API's table
//! editor and an agent's `edit_schema` cannot disagree about what a change means.
//!
//! Three things the tool's schema decides rather than leaving to the model, each
//! because a guess costs a turn or a wrong column: the **type names are an enum
//! built from the live registry**, so `varchar(255)` cannot be invented; a
//! **foreign key is `references: <table>`**, with the storage type taken from the
//! target's primary key rather than asked for; and a **table is created with the
//! key its fields declare** — no `id` is invented (GOALS), so a model that wants
//! one says `primary_key: true` on a field, exactly as an admin ticks the box.

use crate::schema_edit::{
    self, ApplyOptions, FieldSettings, FieldSpec, Grants, Operation, TableSettings,
};
use sc_catalog::{
    ATTR_OWNERSHIP_FORMULA, Catalog, ConstraintKind, DataFieldKind, FieldId, FileOnConflict,
    FileOnDelete, FileStoreId, Table, TableConstraint,
};
use sc_error::{Error, Result};
use sc_types::ArgsExt;
use sc_types::{Attrs, BasicType, RichTypeRef, TypeRef};
use serde_json::{Map, Value as Json, json};

use super::{AdminTool, ToolContext, optional_role};

/// The reading tool's name. Fixed rather than derived, because this surface is
/// configured against no table to derive one from — which is also what makes a
/// second `admin_copilot` on one agent refusable on save (§11.2): the two
/// instances offer the same names, and the collision check refuses that where
/// it is fixable rather than leaving the model to pick between duplicates.
pub const TOOL_DESCRIBE: &str = "describe_schema";
/// The writing tool's name.
pub const TOOL_EDIT: &str = "edit_schema";

/// Who an `edit_schema` batch is made by: an agent, acting for an
/// administrator but not one at the admin UI (TODO-type-hardening H8.5).
const EDIT_SCHEMA_EDITOR: &str = "an agent's edit_schema tool";

/// Describe every table, its access rules and its relationships.
pub(super) struct DescribeSchema;

#[async_trait::async_trait]
impl AdminTool for DescribeSchema {
    fn name(&self) -> &'static str {
        TOOL_DESCRIBE
    }

    fn description(&self, catalog: &Catalog, _grants: &Grants) -> String {
        describe_description(catalog)
    }

    fn parameters(&self) -> Json {
        describe_parameters()
    }

    async fn call(&self, ctx: &ToolContext<'_>, _grants: &Grants, args: &Json) -> Result<Json> {
        describe(ctx.catalog, args)
    }
}

/// Apply an ordered batch of schema operations, or refuse it whole.
pub(super) struct EditSchema;

#[async_trait::async_trait]
impl AdminTool for EditSchema {
    fn name(&self) -> &'static str {
        TOOL_EDIT
    }

    fn description(&self, catalog: &Catalog, grants: &Grants) -> String {
        edit_description(
            grants,
            catalog.primary_info().capabilities().row_level_security,
        )
    }

    fn parameters(&self) -> Json {
        edit_parameters()
    }

    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
        edit(ctx.catalog, grants, args).await
    }
}

/// The optional filter: describe one table instead of all of them.
const ARG_TABLE: &str = "table";

fn describe_description(catalog: &Catalog) -> String {
    let names: Vec<String> = user_tables(catalog).into_iter().map(|t| t.name).collect();
    let listing = match names.is_empty() {
        true => "There are no tables yet.".to_owned(),
        false => format!("The tables are: {}.", names.join(", ")),
    };
    format!(
        "Describe the database schema: every table with its label, description, \
         access rules (the role floors, the ownership formula and whether \
         row-level security enforces it), every field with its type and \
         constraints, and the relationships the foreign keys make — in both \
         directions, so \"what points at clients?\" is answerable. \
         {listing}\n\n\
         This returns **no row data and no row counts**: it describes the shape of \
         the database, never its contents. System tables (`_fd_*`) are not shown."
    )
}

fn describe_parameters() -> Json {
    json!({
        "type": "object",
        "properties": {
            ARG_TABLE: {
                "type": "string",
                "description":
                    "Describe only this table. Omit it to describe every table, \
                     which is what you want before planning a change.",
            },
        },
        "additionalProperties": false,
    })
}

fn describe(catalog: &Catalog, args: &Json) -> Result<Json> {
    let args = super::arguments(args, &[ARG_TABLE])?;
    let only = args
        .get(ARG_TABLE)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let tables = user_tables(catalog);
    if let Some(name) = only
        && !tables.iter().any(|t| t.name == name)
    {
        let names: Vec<String> = tables.iter().map(|t| t.name.clone()).collect();
        return Err(Error::not_found(format!(
            "no table `{name}`; the tables are {}",
            match names.is_empty() {
                true => "none — the database has no tables yet".to_owned(),
                false => names.join(", "),
            }
        )));
    }
    let rls_available = catalog.primary_info().capabilities().row_level_security;
    let described: Vec<Json> = tables
        .iter()
        .filter(|t| only.is_none_or(|name| t.name == name))
        .map(|t| describe_table(t, &tables, rls_available))
        .collect();
    Ok(json!({
        "tables": described,
        "rls_available": rls_available,
    }))
}

/// Every table an admin would call a table: not `_fd_*`, which are invisible to
/// this tool and refused by the other.
fn user_tables(catalog: &Catalog) -> Vec<Table> {
    catalog
        .tables()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| !t.is_hidden())
        .collect()
}

fn describe_table(table: &Table, all: &[Table], rls_available: bool) -> Json {
    // The formula **source**, not merely whether one is in effect: a tool that
    // may write the formula and can only read a boolean has no way to edit one
    // except by overwriting it blind. When the stored formula stopped validating
    // the live one is `None` and the source is still in the attributes, which is
    // exactly the case `ownership_error` explains.
    let formula = table
        .ownership
        .as_ref()
        .map(|f| f.source().to_owned())
        .or_else(|| {
            table
                .attributes
                .get(ATTR_OWNERSHIP_FORMULA)
                .and_then(Json::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let fields: Vec<Json> = table
        .fields
        .iter()
        .map(|f| describe_field(table, f))
        .collect();
    // Both directions, because "what points at `clients`?" is the question a
    // schema is asked and no single field answers it.
    let references: Vec<Json> = table
        .fields
        .iter()
        .filter_map(|f| match &f.kind {
            DataFieldKind::Key {
                target_table,
                target_field,
                ..
            } => Some(json!({
                "field": f.base.name,
                "target_table": target_table,
                "target_field": target_field.0,
            })),
            _ => None,
        })
        .collect();
    let referenced_by: Vec<Json> = all
        .iter()
        .flat_map(|other| {
            other.fields.iter().filter_map(move |f| match &f.kind {
                DataFieldKind::Key {
                    target_table,
                    target_field,
                    ..
                } if *target_table == table.name => Some(json!({
                    "table": other.name,
                    "field": f.base.name,
                    "target_field": target_field.0,
                })),
                _ => None,
            })
        })
        .collect();
    json!({
        "name": table.name,
        "label": table.label,
        "description": table.description,
        "primary_key": table.primary_key,
        "min_role_read": table.access.min_role_read,
        "min_role_write": table.access.min_role_write,
        "ownership_formula": formula,
        "ownership_error": table.ownership_error,
        "rls_enabled": table.rls_enabled,
        // Beside `rls_enabled`, because without it the model proposes RLS to a
        // database that will refuse it, once per conversation.
        "rls_available": rls_available,
        "sync_enabled": table.sync_enabled(),
        "history_enabled": table.history_enabled(),
        "history_min_role": sc_catalog::history::history_min_role(&table.attributes),
        "history_keep_days": sc_catalog::retention::history_keep_days(&table.attributes),
        "fields": fields,
        // Every constraint, so a schema copied from this output keeps its rules:
        // a jointly-unique key, an index and a row formula are the table's, not
        // any one field's. A field that is unique on its own is listed here too,
        // and also says `unique: true`.
        "constraints": table.constraints.iter().map(describe_constraint).collect::<Vec<_>>(),
        "references": references,
        "referenced_by": referenced_by,
    })
}

/// One constraint, in the shape `add_constraint` takes back: `type`, the
/// `fields` it names (or the `formula`), its `name`, and its `error_message`.
fn describe_constraint(constraint: &TableConstraint) -> Json {
    let mut out = Map::new();
    out.insert("name".to_owned(), json!(constraint.name));
    out.insert("type".to_owned(), json!(constraint.kind.type_name()));
    match &constraint.kind {
        ConstraintKind::Unique { fields } => {
            out.insert("fields".to_owned(), json!(fields));
        }
        ConstraintKind::Index {
            fields, expression, ..
        } => {
            out.insert("fields".to_owned(), json!(fields));
            if let Some(expression) = expression {
                out.insert("expression".to_owned(), json!(expression));
            }
        }
        ConstraintKind::FullTextSearch { language } => {
            out.insert("language".to_owned(), json!(language));
        }
        ConstraintKind::Formula { formula } => {
            out.insert("formula".to_owned(), json!(formula));
        }
        ConstraintKind::VectorIndex { field } => {
            out.insert("fields".to_owned(), json!([field]));
        }
    }
    if let Some(message) = &constraint.error_message {
        out.insert("error_message".to_owned(), json!(message));
    }
    Json::Object(out)
}

fn describe_field(table: &Table, field: &sc_catalog::DataField) -> Json {
    let mut out = Map::new();
    out.insert("name".to_owned(), json!(field.base.name));
    out.insert("label".to_owned(), json!(field.base.label));
    out.insert("type".to_owned(), json!(field.base.type_.name()));
    out.insert(
        "storage_type".to_owned(),
        json!(field.base.type_.sql_type()),
    );
    out.insert("required".to_owned(), json!(field.required));
    if let Some(expression) = &field.default_expression {
        out.insert("default".to_owned(), json!(expression));
    }
    // A column's `UNIQUE` is read back as one of the table's constraints, never
    // as the field's own flag, which would say `false` for every one of them.
    out.insert(
        "unique".to_owned(),
        json!(table.is_unique(&field.base.name)),
    );
    if !field.base.attributes.is_empty() {
        out.insert("attributes".to_owned(), json!(field.base.attributes));
    }
    out.insert("primary_key".to_owned(), json!(field.primary_key));
    match &field.kind {
        DataFieldKind::Key {
            target_table,
            target_field,
            summary_field,
        } => {
            out.insert("references".to_owned(), json!(target_table));
            out.insert("references_field".to_owned(), json!(target_field.0));
            if let Some(summary) = summary_field {
                out.insert("summary_field".to_owned(), json!(summary.0));
            }
        }
        DataFieldKind::Calc { expression } => {
            out.insert("calculated".to_owned(), json!(expression));
        }
        DataFieldKind::File {
            store,
            folder,
            mime_allow,
            on_delete,
            on_conflict,
        } => {
            out.insert(ARG_FILE_STORE.to_owned(), json!(store.0));
            if let Some(folder) = folder {
                out.insert(ARG_FILE_FOLDER.to_owned(), json!(folder));
            }
            if !mime_allow.is_empty() {
                out.insert(ARG_FILE_MIME.to_owned(), json!(mime_allow));
            }
            out.insert(ARG_FILE_ON_DELETE.to_owned(), json!(on_delete.as_str()));
            out.insert(ARG_FILE_ON_CONFLICT.to_owned(), json!(on_conflict.as_str()));
        }
        DataFieldKind::Secret => {
            out.insert("secret".to_owned(), json!(true));
        }
        DataFieldKind::Plain => {}
    }
    Json::Object(out)
}

// --- edit_schema ---------------------------------------------------------------

/// The ordered list of operations.
const ARG_OPERATIONS: &str = "operations";
/// Validate the whole batch and apply none of it.
const ARG_DRY_RUN: &str = "dry_run";
/// Makes a field a file field: the file store its files live in.
const ARG_FILE_STORE: &str = "file_store";
/// A file field's folder within its store.
const ARG_FILE_FOLDER: &str = "file_folder";
/// A file field's allowed MIME types.
const ARG_FILE_MIME: &str = "file_mime";
/// What a file field does with its file when the row is deleted.
const ARG_FILE_ON_DELETE: &str = "on_delete";
/// What an upload into a file field does when the name is taken.
const ARG_FILE_ON_CONFLICT: &str = "on_conflict";
/// A field's type settings (`options`, `max_length`, …).
const ARG_ATTRIBUTES: &str = "attributes";
/// What `add_constraint` adds and `drop_constraint` drops.
const ARG_CONSTRAINT: &str = "constraint";

fn edit_description(grants: &Grants, rls_available: bool) -> String {
    let mut allowed: Vec<&str> = Vec::new();
    if grants.create {
        allowed.push("create_table");
    }
    if grants.edit {
        allowed.push("alter_table, rename_table, add_field, alter_field, add_constraint");
    }
    if grants.drop {
        allowed
            .push("drop_field, drop_table, drop_constraint (and `unique: false` on alter_field)");
    }
    let permitted = match allowed.is_empty() {
        true => "You are permitted no operations at all; this tool will refuse \
                 every batch. Say so rather than retrying."
            .to_owned(),
        false => format!("You are permitted: {}.", allowed.join(", ")),
    };
    // TODO-type-hardening H8.5, H8.9: no grant decides access rules; row-level
    // security does. The reply an agent gets is `sc_query::rls_refusal`'s.
    let access = "You may set a table's access rules (`min_role_read`, `min_role_write`, \
                  `ownership_formula`) and turn row-level security on (`rls_enabled: true`). \
                  These change what every other user of this deployment can reach, so say \
                  plainly what you are about to do before you do it. **Once a table has \
                  row-level security, its access rules are a person's**: changing its floors, \
                  its formula or `rls_enabled`, or dropping it, is refused — tell the user what \
                  change you need and why, and they make it in the admin UI. Do not ask for \
                  row-level security to be turned off; the rules can be edited with it on. \
                  Adding and changing its columns, its rows, and the queries and code over it \
                  stay yours.";
    let rls = match rls_available {
        true => "",
        false => {
            " This database cannot enforce row-level security, so \
                  `rls_enabled` will be refused."
        }
    };
    format!(
        "Change the database schema with an ordered list of operations, applied as \
         **one transaction**: either all of them happen or none does, and a refused \
         operation is named by its index in the list. Build a whole connected schema \
         in one call rather than one table per call — a foreign key may point at a \
         table created earlier in the same list, and a formula may name a field \
         added earlier in it.\n\n\
         No primary key is invented: a table has the key its fields declare, so \
         give one field `primary_key: true` — an `int` key numbers itself and a \
         `uuid` key generates itself, and more than one field with it makes a \
         composite key. A table with no key at all is allowed, but cannot be \
         edited row by row or referenced by another table. A foreign key is \
         `references: <table name>` — its storage type comes from that table's \
         primary key and must not be given. A file field (an image, a document) is \
         `{ARG_FILE_STORE}: <file store name>` with no `type`: the column holds the \
         file's path in that store, and the app uploads and serves the file \
         through the store, so do not keep file contents in a `bytes` column.\n\n\
         Rules the database keeps for you: `unique: true` makes one field unique \
         (on `alter_field` too; `unique: false` drops that rule again); \
         `add_constraint` with `constraint: {{type: \"unique\", fields: [a, b]}}` makes \
         several fields unique **together**, and `drop_constraint` removes a \
         constraint by the `name` `{TOOL_DESCRIBE}` lists it under. A fixed list of \
         allowed values (states, priorities) is a field of type `string` with \
         `attributes: {{options: [\"open\", \"closed\"]}}`, refused on every write that \
         sends anything else. A `default` works on optional fields as well as \
         required ones: `created_by` with default `user && user.id` is the writing \
         user's id, and null for a write with no user (a trigger, a scheduled job). \
         A setting an operation does not use is refused, not ignored.\n\n\
         {permitted} {access}{rls}\n\n\
         The change takes effect with no restart, and the result lists any \
         `applications` it moved: a mounted application serving one of these \
         tables is re-projected at once and its generated client rewritten, but \
         **no bundler is run** — so one with `wants_build` is serving a bundle \
         built against the old schema until you rebuild it.\n\n\
         Call `{TOOL_DESCRIBE}` first if you are changing something that already \
         exists; `{ARG_DRY_RUN}` validates a batch and applies none of it."
    )
}

#[allow(clippy::too_many_lines)]
fn edit_parameters() -> Json {
    let types = schema_edit::field_type_names();
    // A flat object with per-`op` optional fields, documented in the
    // descriptions, rather than a `oneOf` discriminated union: providers vary in
    // how well they handle `oneOf` in tool parameters, and Rust validation that
    // names the missing field for the operation at index *n* is a better error
    // than a schema the provider silently flattens.
    let mut parameters = json!({
        "type": "object",
        "properties": {
            ARG_OPERATIONS: {
                "type": "array",
                "description":
                    "The operations, applied in order as one transaction.",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "properties": {
                        "op": {
                            "type": "string",
                            "description": "Which operation this is.",
                            "enum": [
                                "create_table", "alter_table", "rename_table",
                                "add_field", "alter_field", "drop_field",
                                "drop_table", "add_constraint", "drop_constraint",
                            ],
                        },
                        "table": {
                            "type": "string",
                            "description":
                                "The table. For `create_table` this is the new \
                                 table's name: lower-case letters, digits and \
                                 underscores, and usually plural.",
                        },
                        "field": {
                            "type": "string",
                            "description":
                                "The field's name. Required for `add_field`, \
                                 `alter_field` and `drop_field`.",
                        },
                        "rename_to": {
                            "type": "string",
                            "description":
                                "The new name, for `rename_table` (required, and \
                                 alone in its batch): the table keeps its rows, \
                                 fields, settings and row history, and the keys, \
                                 formulas, views, triggers, workflows, agents, \
                                 streams, datasets, models and application table \
                                 subsets that name it follow. Code — code queries, \
                                 code actions, code libraries, an application's \
                                 source — is not rewritten; update it yourself. \
                                 Or rename the field (`alter_field`): its column, its \
                                 settings and its row history follow. Refused while \
                                 a calculated field, a constraint, the ownership \
                                 formula or another table's key names it. Views, \
                                 pages and code that name it must be updated.",
                        },
                        "type": {
                            "type": "string",
                            "description":
                                "The field's type, for `add_field` and \
                                 `alter_field`. Omit it when `references` is \
                                 given — a foreign key takes its type from the \
                                 table it points at.",
                            "enum": types,
                        },
                        "references": {
                            "type": "string",
                            "description":
                                "Make this field a foreign key onto this table's \
                                 primary key (`add_field`).",
                        },
                        "summary_field": {
                            "type": "string",
                            "description":
                                "A field of the referenced table to show as the \
                                 human label when picking a row.",
                        },
                        "expression": {
                            "type": "string",
                            "description":
                                "Make this a calculated field: a JavaScript \
                                 expression over the row's own fields, computed on \
                                 read with no stored column. It cannot use `user` \
                                 or the operation flags.",
                        },
                        "required": {
                            "type": "boolean",
                            "description":
                                "The column rejects nulls (`add_field`, and fields \
                                 of `create_table`). On `alter_field` it makes an \
                                 existing column required, which is refused while \
                                 any row has no value in it, or optional again. A \
                                 primary-key field is always required.",
                        },
                        "unique": {
                            "type": "boolean",
                            "description":
                                "No two rows may have the same value in this field \
                                 (nulls excepted). On `alter_field`, `true` adds the \
                                 rule (refused while rows repeat a value) and `false` \
                                 drops it. For several fields unique together, use \
                                 `add_constraint`.",
                        },
                        "primary_key": {
                            "type": "boolean",
                            "description":
                                "This field is (part of) the table's primary key. \
                                 Nothing invents one, so a table that should be \
                                 editable row by row or referenced by another \
                                 table needs a field that says this. On \
                                 `alter_field` it adds an existing column to the \
                                 key, or takes it out.",
                        },
                        "label": {
                            "type": "string",
                            "description":
                                "A human label for the table or field, shown \
                                 instead of its name.",
                        },
                        "description": {
                            "type": "string",
                            "description": "A human description of the table or field.",
                        },
                        "fields": {
                            "type": "array",
                            "description":
                                "The table's fields, for `create_table`. Include \
                                 the key: one field with `primary_key: true` \
                                 (conventionally `id`, of type `int` or `uuid`), \
                                 because nothing adds one for you.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" },
                                    "type": { "type": "string", "enum": types },
                                    "references": { "type": "string" },
                                    "summary_field": { "type": "string" },
                                    "expression": { "type": "string" },
                                    "required": { "type": "boolean" },
                                    "unique": { "type": "boolean" },
                                    "primary_key": { "type": "boolean" },
                                    "label": { "type": "string" },
                                    "description": { "type": "string" },
                                },
                                "required": ["name"],
                                "additionalProperties": false,
                            },
                        },
                        "min_role_read": {
                            "type": "integer",
                            "description":
                                "Least-privileged role that may read the table's \
                                 rows, 1 (admin) to 100 (anyone). Needs the \
                                 access-rules permission.",
                            "minimum": 1,
                            "maximum": 100,
                        },
                        "min_role_write": {
                            "type": "integer",
                            "description":
                                "Least-privileged role that may write the table's \
                                 rows. Needs the access-rules permission.",
                            "minimum": 1,
                            "maximum": 100,
                        },
                        "ownership_formula": {
                            "type": "string",
                            "description":
                                "A JavaScript expression deciding which rows a \
                                 caller owns, e.g. `owner === user.id`. Empty \
                                 clears it. Needs the access-rules permission.",
                        },
                        "rls_enabled": {
                            "type": "boolean",
                            "description":
                                "Have the database enforce the ownership formula \
                                 with row-level-security policies. Turning it off \
                                 removes that enforcement. Needs the access-rules \
                                 permission.",
                        },
                        "sync_enabled": {
                            "type": "boolean",
                            "description":
                                "Record the table's changes for offline sync. Needs \
                                 a primary key of one column; offline inserts need \
                                 a uuid key.",
                        },
                        "history_enabled": {
                            "type": "boolean",
                            "description":
                                "Keep every version of the table's rows, so a row's \
                                 history can be listed and an old version restored. \
                                 Off by default; needs a primary key of one column.",
                        },
                        "history_min_role": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 100,
                            "description":
                                "The least-privileged role that may read the \
                                 table's history; 1 (admin only) by default. Each \
                                 row is still checked against the read rule. Kept \
                                 only while history is on; needs the access-rules \
                                 permission.",
                        },
                        "history_keep_days": {
                            "type": "integer",
                            "minimum": 0,
                            "description":
                                "How many days the table's history keeps a change \
                                 (and a deleted row's files); 0, the default, keeps \
                                 it forever. Kept only while history is on.",
                        },
                    },
                    "required": ["op", "table"],
                    "additionalProperties": false,
                },
            },
            ARG_DRY_RUN: {
                "type": "boolean",
                "description":
                    "Validate the whole batch and apply none of it. The same \
                     refusals, no changes.",
            },
        },
        "required": [ARG_OPERATIONS],
        "additionalProperties": false,
    });
    add_file_properties(&mut parameters);
    add_default_property(&mut parameters);
    add_attributes_property(&mut parameters);
    add_constraint_property(&mut parameters);
    parameters
}

/// The `attributes` property, on an operation and on a `create_table` field —
/// added after the fact for the reason [`add_file_properties`] is.
fn add_attributes_property(parameters: &mut Json) {
    let schema = json!({
        "type": "object",
        "description":
            "The type's settings, as `listFieldTypes` lists them for each type, for \
             `add_field`, `alter_field` (which replaces them all) and fields of \
             `create_table`. For `string`: `options` (the only values allowed, \
             e.g. [\"open\", \"in_progress\", \"closed\"]; on `alter_field` refused \
             while a row holds another value), `max_length` and `regex`. For \
             `integer`: `min` and `max`. A basic type such as `text` or `int` has \
             none: use `string` or `integer` instead.",
    });
    let item = &mut parameters["properties"][ARG_OPERATIONS]["items"];
    item["properties"][ARG_ATTRIBUTES] = schema.clone();
    item["properties"]["fields"]["items"]["properties"][ARG_ATTRIBUTES] = schema;
}

/// The `constraint` property of `add_constraint` and `drop_constraint`.
fn add_constraint_property(parameters: &mut Json) {
    parameters["properties"][ARG_OPERATIONS]["items"]["properties"][ARG_CONSTRAINT] = json!({
        "type": "object",
        "description":
            "For `add_constraint`: `type` and what it needs — `unique` with `fields` \
             (unique together, e.g. [\"document_id\", \"version_no\"]; refused while rows \
             repeat a combination), `index` with `fields` (one field: faster \
             searching and joining), `formula` with `formula` (a JavaScript \
             expression every row must satisfy, e.g. `end_date >= start_date`) and a \
             short `name`, or `full_text_search` with an optional `language`. \
             `error_message` is shown when a write breaks it. For \
             `drop_constraint`: `name`, as `describe_schema` lists it.",
        "properties": {
            "type": {
                "type": "string",
                "enum": ["unique", "index", "formula", "full_text_search"],
            },
            "fields": { "type": "array", "items": { "type": "string" } },
            "formula": { "type": "string" },
            "language": { "type": "string" },
            "name": { "type": "string" },
            "error_message": { "type": "string" },
        },
        "additionalProperties": false,
    });
}

/// The `default` property, on an operation and on a `create_table` field alike —
/// added after the fact for the reason [`add_file_properties`] is.
fn add_default_property(parameters: &mut Json) {
    let schema = json!({
        "type": "string",
        "description":
            "A default value, for an optional field as well as a required one: a \
             JavaScript expression evaluated when an insert leaves the field out or \
             sends null. It sees `user` (the user writing the row, null when there \
             is none, as in a trigger or a scheduled job) and the row's other \
             fields. On an optional field, an expression that gives nothing leaves \
             the field null: `user && user.id` fills a `created_by` field when a \
             user writes the row and leaves it empty otherwise. On a required field \
             it must give a value. `title.toUpperCase()` is another example. An \
             empty string clears it on `alter_field`.",
    });
    let item = &mut parameters["properties"][ARG_OPERATIONS]["items"];
    item["properties"]["default"] = schema.clone();
    item["properties"]["fields"]["items"]["properties"]["default"] = schema;
}

/// The three properties that make a field a file field, on an operation and on
/// a `create_table` field alike. Added after the fact only because one more
/// level of `json!` exceeds the macro's recursion limit.
fn add_file_properties(parameters: &mut Json) {
    let file = [
        (
            ARG_FILE_STORE,
            json!({
                "type": "string",
                "description":
                    "Make this a file field whose files live in this file store \
                     (`add_field`, `alter_field`, fields of `create_table`). Give \
                     no `type`: the column holds the file's path in the store.",
            }),
        ),
        (
            ARG_FILE_FOLDER,
            json!({
                "type": "string",
                "description": "A file field's folder within its store.",
            }),
        ),
        (
            ARG_FILE_MIME,
            json!({
                "type": "array",
                "items": { "type": "string" },
                "description":
                    "A file field's allowed MIME types, such as `image/*`; none \
                     means any.",
            }),
        ),
        (
            ARG_FILE_ON_DELETE,
            json!({
                "type": "string",
                "enum": ["keep", "remove_file"],
                "description":
                    "A file field's stored file when its row is deleted: `keep` \
                     (default) leaves it in the store, `remove_file` deletes it once \
                     the delete commits, unless another row still references it. On \
                     `alter_field` it may be given without `file_store`.",
            }),
        ),
        (
            ARG_FILE_ON_CONFLICT,
            json!({
                "type": "string",
                "enum": ["overwrite", "suffix"],
                "description":
                    "An upload into a file field whose name is already taken in the \
                     folder: `overwrite` (default) replaces it, `suffix` stores it as \
                     `name_1.ext`, `name_2.ext`, …. On `alter_field` it may be given \
                     without `file_store`.",
            }),
        ),
    ];
    let item = &mut parameters["properties"][ARG_OPERATIONS]["items"];
    for (name, schema) in &file {
        item["properties"][*name] = schema.clone();
        item["properties"]["fields"]["items"]["properties"][*name] = schema.clone();
    }
}

async fn edit(catalog: &Catalog, grants: &Grants, args: &Json) -> Result<Json> {
    let args = super::arguments(args, &[ARG_OPERATIONS, ARG_DRY_RUN])?;
    let items = match args.get(ARG_OPERATIONS) {
        Some(Json::Array(items)) if !items.is_empty() => items.clone(),
        Some(Json::Array(_)) | None => {
            return Err(Error::invalid(format!(
                "`{ARG_OPERATIONS}` must list at least one operation"
            )));
        }
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{ARG_OPERATIONS}` should be a list of operations, got {other}"
            )));
        }
    };
    let dry_run = match args.get(ARG_DRY_RUN) {
        None | Some(Json::Null) => false,
        Some(Json::Bool(b)) => *b,
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{ARG_DRY_RUN}` should be true or false, got {other}"
            )));
        }
    };

    let mut operations = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        operations.push(
            complete_file_alter(catalog, item)
                .and_then(|item| parse_operation(&item))
                .map_err(|e| Error::invalid(format!("operation {index}: {e}")))?,
        );
    }

    check_file_stores(catalog, &operations)?;
    check_attributes(catalog, &operations)?;

    let applied = schema_edit::apply(
        catalog,
        &operations,
        &ApplyOptions {
            grants: *grants,
            dry_run,
            // An agent (an MCP client, `admin_copilot`, a coding agent): never a
            // person, so an RLS table's access rules are not its to change
            // (TODO-type-hardening H8.5).
            editor: sc_catalog::Editor::Delegated(EDIT_SCHEMA_EDITOR),
        },
    )
    .await?;
    let mut notes = applied.notes.clone();
    notes.extend(rebuild_notes(&applied.applications));
    Ok(json!({
        "applied": !applied.dry_run,
        "dry_run": applied.dry_run,
        "tables_created": applied.tables_created,
        "tables_altered": applied.tables_altered,
        "tables_dropped": applied.tables_dropped,
        "tables_renamed": applied
            .tables_renamed
            .iter()
            .map(|(from, to)| json!({ "from": from, "to": to }))
            .collect::<Vec<Json>>(),
        "fields_added": applied.fields_added,
        "fields_altered": applied.fields_altered,
        "fields_dropped": applied.fields_dropped,
        "constraints_added": applied.constraints_added,
        "constraints_dropped": applied.constraints_dropped,
        "applications": applied
            .applications
            .iter()
            .map(|app| {
                json!({
                    "id": app.id,
                    "subdomain": app.subdomain,
                    "wants_build": app.wants_build,
                })
            })
            .collect::<Vec<Json>>(),
        "notes": notes,
    }))
}

/// The sentence the applications half of the result needs, or none.
///
/// The structured list says *what happened*; this says **what to do about it**,
/// which is the difference between a report and an instruction. Re-projection
/// took effect at once and ran no bundler, so an application served from a built
/// bundle is now serving one generated against the previous schema — and a model
/// that is not told will not guess. It is a note rather than prose in the
/// description because it is true only of the batch that just ran.
fn rebuild_notes(applications: &[sc_catalog::ReprojectedApp]) -> Vec<String> {
    let names = |apps: &[&sc_catalog::ReprojectedApp]| {
        apps.iter()
            .map(|a| format!("`{}`", a.subdomain))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (built, unbuilt): (Vec<_>, Vec<_>) = applications.iter().partition(|a| a.wants_build);
    let mut notes = Vec::new();
    if !built.is_empty() {
        notes.push(format!(
            "Re-projected, with the generated client rewritten: {}. No bundler was \
             run and these are served from a built bundle, so call \
             `buildApplication` with the `id` above to rebuild each against the \
             schema this batch left.",
            names(&built)
        ));
    }
    if !unbuilt.is_empty() {
        notes.push(format!(
            "Re-projected, with nothing to build: {}.",
            names(&unbuilt)
        ));
    }
    notes
}

/// Turn one wire item into an [`Operation`], naming the field the operation is
/// missing rather than reporting a shape mismatch — the whole reason the items
/// are a flat object rather than a `oneOf`.
fn parse_operation(item: &Json) -> Result<Operation> {
    let obj = item
        .as_object()
        .ok_or_else(|| Error::invalid(format!("should be an object, got {item}")))?;
    let op = obj
        .get("op")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::invalid("needs an `op`"))?
        .trim();
    let table = obj
        .get("table")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::invalid(format!("`{op}` needs a `table`")))?
        .to_owned();
    refuse_unused_settings(obj, op)?;

    match op {
        "create_table" => {
            let fields = match obj.get("fields") {
                None | Some(Json::Null) => Vec::new(),
                Some(Json::Array(items)) => items
                    .iter()
                    .map(parse_field_spec)
                    .collect::<Result<Vec<_>>>()?,
                Some(other) => {
                    return Err(Error::invalid(format!(
                        "`fields` should be a list of field objects, got {other}"
                    )));
                }
            };
            Ok(Operation::CreateTable {
                name: table,
                // The copilot builds Saltcorn's own schema. A table on a
                // *connected* database is an admin's deliberate choice about
                // somebody else's database, made in the New table dialog where
                // the connection is named and visible — not something to infer
                // from a sentence typed at an agent.
                database: String::new(),
                settings: parse_table_settings(obj)?,
                fields,
            })
        }
        "alter_table" => Ok(Operation::AlterTable {
            table,
            settings: parse_table_settings(obj)?,
        }),
        "add_field" => {
            // The item *is* the field, with `field` naming it — one flat shape
            // per operation rather than an object nested inside an object whose
            // sibling keys mean something else.
            let mut spec = parse_field_spec(item)?;
            spec.name = require_field(obj, op)?;
            Ok(Operation::AddField { table, field: spec })
        }
        "alter_field" => Ok(Operation::AlterField {
            table,
            field: require_field(obj, op)?,
            settings: parse_field_settings(obj)?,
        }),
        "drop_field" => Ok(Operation::DropField {
            table,
            field: require_field(obj, op)?,
        }),
        "drop_table" => Ok(Operation::DropTable { table }),
        "rename_table" => Ok(Operation::RenameTable {
            table,
            to: obj
                .optional_string("rename_to")?
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| Error::invalid("`rename_table` needs a `rename_to`"))?,
        }),
        "add_constraint" => {
            let spec = constraint_object(obj, op)?;
            let (given_name, constraint) = parse_constraint(spec)?;
            Ok(Operation::AddConstraint {
                table,
                given_name,
                constraint,
            })
        }
        "drop_constraint" => Ok(Operation::DropConstraint {
            table,
            name: constraint_object(obj, op)?
                .optional_string("name")?
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty())
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "`drop_constraint` needs `{ARG_CONSTRAINT}: {{name: …}}`, the name \
                         `{TOOL_DESCRIBE}` lists the constraint under"
                    ))
                })?,
        }),
        other => Err(Error::invalid(format!(
            "unknown `op` `{other}`; it is one of {}",
            OPS.join(", ")
        ))),
    }
}

/// The operations, in the order they are listed in messages.
const OPS: &[&str] = &[
    "create_table",
    "alter_table",
    "rename_table",
    "add_field",
    "alter_field",
    "drop_field",
    "drop_table",
    "add_constraint",
    "drop_constraint",
];

/// A table's settings, on `create_table` and `alter_table`.
const TABLE_SETTINGS: &[&str] = &[
    "label",
    "description",
    "min_role_read",
    "min_role_write",
    "ownership_formula",
    "rls_enabled",
    "sync_enabled",
    "history_enabled",
    "history_min_role",
    "history_keep_days",
];

/// A field's settings, on `add_field` and `alter_field` (and, less `field`, on
/// a field of `create_table`).
const FIELD_SETTINGS: &[&str] = &[
    "field",
    "type",
    "references",
    "summary_field",
    "expression",
    "required",
    "unique",
    "primary_key",
    "label",
    "description",
    "default",
    ARG_ATTRIBUTES,
    ARG_FILE_STORE,
    ARG_FILE_FOLDER,
    ARG_FILE_MIME,
    ARG_FILE_ON_DELETE,
    ARG_FILE_ON_CONFLICT,
];

/// The settings operation `op` reads, besides `op` and `table`; `None` for an
/// unknown operation, which is reported as one.
fn op_settings(op: &str) -> Option<Vec<&'static str>> {
    Some(match op {
        "create_table" => [TABLE_SETTINGS, &["fields"]].concat(),
        "alter_table" => TABLE_SETTINGS.to_vec(),
        "rename_table" => vec!["rename_to"],
        "add_field" => FIELD_SETTINGS.to_vec(),
        "alter_field" => [FIELD_SETTINGS, &["rename_to"]].concat(),
        "drop_field" => vec!["field"],
        "drop_table" => Vec::new(),
        "add_constraint" | "drop_constraint" => vec![ARG_CONSTRAINT],
        _ => return None,
    })
}

/// Refuse a setting operation `op` does not read, by name. Every setting is
/// read by *some* operation, so the tool's schema lets it through, and doing
/// nothing with it is how `alter_field unique: false` once reported success
/// and changed nothing. A null counts as not sent.
fn refuse_unused_settings(obj: &Map<String, Json>, op: &str) -> Result<()> {
    let Some(settings) = op_settings(op) else {
        // The `match` below names the operations.
        return Ok(());
    };
    let unused: Vec<String> = obj
        .iter()
        .filter(|(key, value)| {
            !value.is_null()
                && key.as_str() != "op"
                && key.as_str() != "table"
                && !settings.contains(&key.as_str())
        })
        .map(|(key, _)| format!("`{key}`"))
        .collect();
    if unused.is_empty() {
        return Ok(());
    }
    let takes = match settings.is_empty() {
        true => "nothing but `table`".to_owned(),
        false => settings
            .iter()
            .map(|s| format!("`{s}`"))
            .collect::<Vec<_>>()
            .join(", "),
    };
    Err(Error::invalid(format!(
        "`{op}` does not use {}; it takes {takes}. Nothing was changed.",
        unused.join(", ")
    )))
}

/// The `constraint` object of `add_constraint` and `drop_constraint`.
fn constraint_object<'a>(obj: &'a Map<String, Json>, op: &str) -> Result<&'a Map<String, Json>> {
    match obj.get(ARG_CONSTRAINT) {
        Some(Json::Object(spec)) => Ok(spec),
        None | Some(Json::Null) => Err(Error::invalid(format!(
            "`{op}` needs a `{ARG_CONSTRAINT}` object"
        ))),
        Some(other) => Err(Error::invalid(format!(
            "`{ARG_CONSTRAINT}` should be an object, got {other}"
        ))),
    }
}

/// Read an `add_constraint`'s `constraint` into the short name given (a row
/// constraint's) and the constraint, its name left to be derived. The `type`
/// decides which settings are read, and a missing one is named.
fn parse_constraint(spec: &Map<String, Json>) -> Result<(String, TableConstraint)> {
    let fields = || -> Result<Vec<String>> {
        match spec.get("fields") {
            Some(Json::Array(items)) => items
                .iter()
                .map(|f| {
                    f.as_str()
                        .map(|f| f.trim().to_owned())
                        .filter(|f| !f.is_empty())
                        .ok_or_else(|| Error::invalid("`fields` should be a list of field names"))
                })
                .collect(),
            _ => Err(Error::invalid(
                "this constraint needs `fields`, a list of field names",
            )),
        }
    };
    let type_name = spec
        .optional_string("type")?
        .map(|t| t.trim().to_owned())
        .unwrap_or_default();
    let kind = match type_name.as_str() {
        "unique" => ConstraintKind::Unique { fields: fields()? },
        "index" => ConstraintKind::Index {
            fields: fields()?,
            expression: None,
            method: "btree".to_owned(),
        },
        "formula" => ConstraintKind::Formula {
            formula: spec
                .optional_string("formula")?
                .map(|f| f.trim().to_owned())
                .filter(|f| !f.is_empty())
                .ok_or_else(|| Error::invalid("a `formula` constraint needs a `formula`"))?,
        },
        "full_text_search" => ConstraintKind::FullTextSearch {
            language: spec
                .optional_string("language")?
                .map(|l| l.trim().to_owned())
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| "english".to_owned()),
        },
        "" => {
            return Err(Error::invalid(
                "the constraint needs a `type`: `unique`, `index`, `formula` or \
                 `full_text_search`",
            ));
        }
        other => {
            return Err(Error::invalid(format!(
                "`{other}` is not a constraint type; it is one of `unique`, `index`, \
                 `formula` or `full_text_search`"
            )));
        }
    };
    let mut constraint = TableConstraint::new(String::new(), kind);
    constraint.error_message = spec
        .optional_string("error_message")?
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty());
    let name = spec
        .optional_string("name")?
        .map(|n| n.trim().to_owned())
        .unwrap_or_default();
    Ok((name, constraint))
}

/// Refuse a file field whose store does not exist, naming the ones that do.
///
/// Here rather than in the schema editor because the editor's projection is of
/// tables: a file store is not part of the schema, and a store that is missing
/// would otherwise be found only when the first upload fails.
fn check_file_stores(catalog: &Catalog, operations: &[Operation]) -> Result<()> {
    let kinds = operations.iter().flat_map(|op| match op {
        Operation::CreateTable { fields, .. } => fields.iter().map(|f| &f.kind).collect(),
        Operation::AddField { field, .. } => vec![&field.kind],
        Operation::AlterField { settings, .. } => settings.kind.iter().collect(),
        _ => Vec::new(),
    });
    for kind in kinds {
        if let DataFieldKind::File { store, .. } = kind
            && !catalog.has_file_store(&store.0)?
        {
            let names = catalog.file_store_names()?;
            return Err(Error::invalid(format!(
                "there is no file store `{}`; the file stores are {}. Create one with \
                 `create_file_store` first. Nothing was changed.",
                store.0,
                match names.is_empty() {
                    true => "none yet".to_owned(),
                    false => names
                        .iter()
                        .map(|n| format!("`{n}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                }
            )));
        }
    }
    Ok(())
}

fn require_field(obj: &Map<String, Json>, op: &str) -> Result<String> {
    obj.get("field")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::invalid(format!("`{op}` needs a `field`")))
}

fn parse_table_settings(obj: &Map<String, Json>) -> Result<TableSettings> {
    Ok(TableSettings {
        label: obj.optional_string("label")?,
        description: obj.optional_string("description")?,
        min_role_read: optional_role(obj, "min_role_read")?,
        min_role_write: optional_role(obj, "min_role_write")?,
        ownership_formula: obj.optional_string("ownership_formula")?,
        rls_enabled: obj.optional_bool("rls_enabled")?,
        sync_enabled: obj.optional_bool("sync_enabled")?,
        history_enabled: obj.optional_bool("history_enabled")?,
        history_min_role: optional_role(obj, "history_min_role")?,
        history_keep_days: match obj.get("history_keep_days") {
            None | Some(Json::Null) => None,
            Some(days) => Some(
                days.as_u64()
                    .and_then(|d| u32::try_from(d).ok())
                    .ok_or_else(|| {
                        Error::invalid("`history_keep_days` must be a whole number of days")
                    })?,
            ),
        },
    })
}

fn parse_field_settings(obj: &Map<String, Json>) -> Result<FieldSettings> {
    let kind = field_kind(obj)?;
    Ok(FieldSettings {
        name: obj.optional_string("rename_to")?,
        label: obj.optional_string("label")?,
        description: obj.optional_string("description")?,
        type_name: obj.optional_string("type")?,
        kind,
        attributes: optional_attributes(obj)?,
        primary_key: obj.optional_bool("primary_key")?,
        required: obj.optional_bool("required")?,
        unique: obj.optional_bool("unique")?,
        default_expression: obj.optional_string("default")?,
    })
}

/// The `attributes` object, or `None` when it is not sent.
fn optional_attributes(obj: &Map<String, Json>) -> Result<Option<Attrs>> {
    match obj.get(ARG_ATTRIBUTES) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Object(attrs)) => Ok(Some(attrs.clone())),
        Some(other) => Err(Error::invalid(format!(
            "`{ARG_ATTRIBUTES}` should be an object such as {{\"options\": [\"open\", \
             \"closed\"]}}, got {other}"
        ))),
    }
}

/// Refuse `attributes` the field's type does not have, naming the ones it does.
///
/// Here rather than in the schema editor, which stores what it is given: the
/// admin UI only ever sends the attributes its form was built from, while a
/// model can send `options` on a `text` field, which nothing would enforce.
/// The type is the one the operation gives, or, for an `alter_field` that
/// gives none, the field's type as it is.
fn check_attributes(catalog: &Catalog, operations: &[Operation]) -> Result<()> {
    for op in operations {
        let checks: Vec<(String, Option<TypeRef>, &Attrs)> = match op {
            Operation::CreateTable { fields, .. } => fields
                .iter()
                .map(|f| (f.name.clone(), named_type(&f.type_name), &f.attributes))
                .collect(),
            Operation::AddField { field, .. } => vec![(
                field.name.clone(),
                named_type(&field.type_name),
                &field.attributes,
            )],
            Operation::AlterField {
                table,
                field,
                settings,
            } => match &settings.attributes {
                None => Vec::new(),
                Some(attrs) => {
                    let type_ = match settings.type_name.as_deref().map(str::trim) {
                        Some(name) if !name.is_empty() => named_type(name),
                        Some(_) => None,
                        None => catalog
                            .get(table)?
                            .and_then(|t| t.field(field).map(|f| f.base.type_.clone())),
                    };
                    vec![(field.clone(), type_, attrs)]
                }
            },
            _ => Vec::new(),
        };
        for (field, type_, attrs) in checks {
            if attrs.is_empty() {
                continue;
            }
            let (type_label, known) = match &type_ {
                Some(TypeRef::Rich(rich)) => (
                    rich.name().to_owned(),
                    rich.attributes()
                        .iter()
                        .map(|a| a.base.name.clone())
                        .collect::<Vec<_>>(),
                ),
                Some(TypeRef::Basic(BasicType::Vector(n))) => (
                    format!("vector({n})"),
                    sc_types::vector::vector_attributes()
                        .into_iter()
                        .map(|a| a.base.name)
                        .collect(),
                ),
                Some(TypeRef::Basic(basic)) => (basic.name().to_owned(), Vec::new()),
                // An unknown type is the schema editor's to name.
                None => continue,
            };
            let unknown: Vec<String> = attrs
                .keys()
                .filter(|k| !known.contains(k))
                .map(|k| format!("`{k}`"))
                .collect();
            if unknown.is_empty() {
                continue;
            }
            let has = match known.is_empty() {
                true => "none — for allowed values or a length limit use type `string`, for \
                         a range `integer`"
                    .to_owned(),
                false => known
                    .iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            };
            return Err(Error::invalid(format!(
                "field `{field}` is of type `{type_label}`, which has no attribute {}; \
                 its attributes are {has}. Nothing was changed.",
                unknown.join(", ")
            )));
        }
    }
    Ok(())
}

/// The type a `type` name gives, or `None` for none or an unknown one.
fn named_type(name: &str) -> Option<TypeRef> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if let Ok(rich) = RichTypeRef::resolve(name) {
        return Some(TypeRef::Rich(rich));
    }
    schema_edit::resolve_field_type(name).ok().map(|(t, _)| t)
}

fn parse_field_spec(item: &Json) -> Result<FieldSpec> {
    let obj = item
        .as_object()
        .ok_or_else(|| Error::invalid(format!("a field should be an object, got {item}")))?;
    let name = obj
        .get("name")
        .and_then(Json::as_str)
        .or_else(|| obj.get("field").and_then(Json::as_str))
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    Ok(FieldSpec {
        name,
        type_name: obj.optional_string("type")?.unwrap_or_default(),
        label: obj.optional_string("label")?.unwrap_or_default(),
        description: obj.optional_string("description")?.unwrap_or_default(),
        required: obj.optional_bool("required")?.unwrap_or(false),
        unique: obj.optional_bool("unique")?.unwrap_or(false),
        primary_key: obj.optional_bool("primary_key")?.unwrap_or(false),
        kind: field_kind(obj)?.unwrap_or(DataFieldKind::Plain),
        attributes: optional_attributes(obj)?.unwrap_or_default(),
        default_expression: obj.optional_string("default")?,
    })
}

/// A file field's enumerated setting: its default when absent or null.
fn file_choice<T: Default>(
    obj: &Map<String, Json>,
    key: &str,
    parse: fn(&str) -> Option<T>,
) -> Result<T> {
    match obj.optional_string(key)? {
        None => Ok(T::default()),
        Some(text) => parse(&text).ok_or_else(|| {
            Error::invalid(format!(
                "`{key}` does not take `{text}`; the choices are {}",
                match key {
                    ARG_FILE_ON_DELETE => "`keep` and `remove_file`",
                    _ => "`overwrite` and `suffix`",
                }
            ))
        }),
    }
}

/// An `alter_field` that changes a file field's settings without restating
/// `file_store`, completed from the field as it is: the file settings it does
/// not mention keep their current values. Anything else is left as it came.
fn complete_file_alter(catalog: &Catalog, item: &Json) -> Result<Json> {
    let Some(obj) = item.as_object() else {
        return Ok(item.clone());
    };
    let mentions = [
        ARG_FILE_FOLDER,
        ARG_FILE_MIME,
        ARG_FILE_ON_DELETE,
        ARG_FILE_ON_CONFLICT,
    ]
    .iter()
    .any(|k| obj.get(*k).is_some_and(|v| !v.is_null()));
    if obj.get("op").and_then(Json::as_str) != Some("alter_field")
        || obj.get(ARG_FILE_STORE).is_some_and(|v| !v.is_null())
        || !mentions
    {
        return Ok(item.clone());
    }
    let (Some(table), Some(field)) = (
        obj.get("table").and_then(Json::as_str),
        obj.get("field").and_then(Json::as_str),
    ) else {
        return Ok(item.clone());
    };
    let Some(DataFieldKind::File {
        store,
        folder,
        mime_allow,
        on_delete,
        on_conflict,
    }) = catalog
        .get(table.trim())?
        .and_then(|t| t.field(field.trim()).map(|f| f.kind.clone()))
    else {
        return Ok(item.clone());
    };
    let mut obj = obj.clone();
    obj.insert(ARG_FILE_STORE.to_owned(), json!(store.0));
    let mut keep = |key: &str, value: Json| {
        obj.entry(key.to_owned())
            .and_modify(|v| {
                if v.is_null() {
                    *v = value.clone();
                }
            })
            .or_insert(value);
    };
    keep(ARG_FILE_FOLDER, json!(folder));
    keep(ARG_FILE_MIME, json!(mime_allow));
    keep(ARG_FILE_ON_DELETE, json!(on_delete.as_str()));
    keep(ARG_FILE_ON_CONFLICT, json!(on_conflict.as_str()));
    Ok(Json::Object(obj))
}

/// A field's kind from the flat item: `references` makes it a foreign key,
/// `expression` a calculated field and `file_store` a file field, and more than
/// one of them is a contradiction worth naming rather than resolving by
/// precedence.
fn field_kind(obj: &Map<String, Json>) -> Result<Option<DataFieldKind>> {
    let references = obj
        .optional_string("references")?
        .filter(|s| !s.trim().is_empty());
    let expression = obj
        .optional_string("expression")?
        .filter(|s| !s.trim().is_empty());
    let file_store = obj
        .optional_string(ARG_FILE_STORE)?
        .filter(|s| !s.trim().is_empty());
    if obj.get("type").and_then(Json::as_str).map(str::trim) == Some("file") {
        return Err(Error::invalid(format!(
            "`file` is not a type; a file field is one with `{ARG_FILE_STORE}` naming \
             the file store its files live in, and it needs no `type`"
        )));
    }
    if let Some(store) = file_store {
        if references.is_some() || expression.is_some() {
            return Err(Error::invalid(
                "a field is one of a reference, a calculated expression or a file, \
                 not several",
            ));
        }
        let mime_allow = match obj.get(ARG_FILE_MIME) {
            None | Some(Json::Null) => Vec::new(),
            Some(Json::Array(items)) => items
                .iter()
                .map(|m| {
                    m.as_str().map(|m| m.trim().to_owned()).ok_or_else(|| {
                        Error::invalid(format!("`{ARG_FILE_MIME}` should be a list of MIME types"))
                    })
                })
                .collect::<Result<_>>()?,
            Some(other) => {
                return Err(Error::invalid(format!(
                    "`{ARG_FILE_MIME}` should be a list of MIME types, got {other}"
                )));
            }
        };
        return Ok(Some(DataFieldKind::File {
            store: FileStoreId(store.trim().to_owned()),
            folder: obj
                .optional_string(ARG_FILE_FOLDER)?
                .map(|f| f.trim().trim_matches('/').to_owned())
                .filter(|f| !f.is_empty()),
            mime_allow,
            on_delete: file_choice(obj, ARG_FILE_ON_DELETE, FileOnDelete::parse)?,
            on_conflict: file_choice(obj, ARG_FILE_ON_CONFLICT, FileOnConflict::parse)?,
        }));
    }
    for key in [
        ARG_FILE_FOLDER,
        ARG_FILE_MIME,
        ARG_FILE_ON_DELETE,
        ARG_FILE_ON_CONFLICT,
    ] {
        if obj.get(key).is_some_and(|v| !v.is_null()) {
            return Err(Error::invalid(format!(
                "`{key}` is a file field's setting; give `{ARG_FILE_STORE}` too to make \
                 this a file field"
            )));
        }
    }
    match (references, expression) {
        (Some(_), Some(_)) => Err(Error::invalid(
            "a field is either a reference or a calculated expression, not both",
        )),
        (Some(target), None) => Ok(Some(DataFieldKind::Key {
            target_table: target.trim().to_owned(),
            // Empty: the schema editor resolves it to the target's primary key,
            // so the model never has to know (or guess) the column's name.
            target_field: FieldId(String::new()),
            summary_field: obj
                .optional_string("summary_field")?
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .map(FieldId),
        })),
        (None, Some(expression)) => Ok(Some(DataFieldKind::Calc {
            expression: expression.trim().to_owned(),
        })),
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_types::Role;

    #[test]
    fn a_file_field_is_named_by_its_store_and_file_is_not_a_type() {
        let item = json!({
            "op": "add_field", "table": "slides", "field": "picture",
            "file_store": " media ", "file_folder": "/slides/", "file_mime": ["image/png"],
        });
        let Operation::AddField { field, .. } = parse_operation(&item).unwrap() else {
            panic!("an add_field");
        };
        assert!(field.type_name.is_empty());
        assert_eq!(
            field.kind,
            DataFieldKind::File {
                store: FileStoreId("media".to_owned()),
                folder: Some("slides".to_owned()),
                mime_allow: vec!["image/png".to_owned()],
                on_delete: Default::default(),
                on_conflict: Default::default(),
            }
        );

        let as_type = json!({
            "op": "add_field", "table": "slides", "field": "picture", "type": "file",
        });
        let err = parse_operation(&as_type).unwrap_err().to_string();
        assert!(err.contains("`file` is not a type"), "{err}");

        let both = json!({
            "op": "add_field", "table": "slides", "field": "picture",
            "file_store": "media", "references": "clients",
        });
        assert!(parse_operation(&both).is_err());
    }

    #[test]
    fn a_reference_needs_no_type_and_no_target_column() {
        let item = json!({
            "op": "add_field", "table": "matters",
            "field": "client", "references": "clients",
        });
        let Operation::AddField { table, field } = parse_operation(&item).unwrap() else {
            panic!("an add_field");
        };
        assert_eq!(table, "matters");
        assert_eq!(field.name, "client");
        // No type was given and none was invented: the storage type comes from
        // the target's primary key, resolved by the schema editor.
        assert!(field.type_name.is_empty());
        match field.kind {
            DataFieldKind::Key {
                target_table,
                target_field,
                ..
            } => {
                assert_eq!(target_table, "clients");
                assert!(target_field.0.is_empty());
            }
            other => panic!("a key, got {other:?}"),
        }
    }

    #[test]
    fn an_operation_missing_its_field_is_told_which_field() {
        let err = parse_operation(&json!({ "op": "drop_field", "table": "clients" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`drop_field` needs a `field`"), "{err}");
        let err = parse_operation(&json!({ "op": "add_field" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs a `table`"), "{err}");
        let err = parse_operation(&json!({ "op": "invent_table", "table": "x" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("create_table"), "{err}");
    }

    #[test]
    fn omitted_settings_are_none_rather_than_reset() {
        let Operation::AlterTable { settings, .. } = parse_operation(&json!({
            "op": "alter_table", "table": "clients", "min_role_read": 40,
        }))
        .unwrap() else {
            panic!("an alter_table");
        };
        assert_eq!(settings.min_role_read, Some(Role::of(40)));
        // The three settings the caller did not name are `None` — "leave it" —
        // which is the whole divergence from `updateTable`'s whole-object
        // contract (§13.1).
        assert_eq!(settings.min_role_write, None);
        assert_eq!(settings.ownership_formula, None);
        assert_eq!(settings.rls_enabled, None);
        assert_eq!(settings.label, None);
    }

    #[test]
    fn the_history_switch_reaches_agents_beside_sync() {
        let Operation::AlterTable { settings, .. } = parse_operation(&json!({
            "op": "alter_table", "table": "clients", "history_enabled": true,
        }))
        .unwrap() else {
            panic!("an alter_table");
        };
        assert_eq!(settings.history_enabled, Some(true));
        assert_eq!(settings.sync_enabled, None);
        // And the tool's schema offers it, or the model never learns to ask.
        let params = edit_parameters();
        let props = &params["properties"][ARG_OPERATIONS]["items"]["properties"];
        assert_eq!(props["history_enabled"]["type"], json!("boolean"));
        assert_eq!(props["sync_enabled"]["type"], json!("boolean"));
    }

    #[test]
    fn alter_field_reads_unique_and_attributes_rather_than_dropping_them() {
        let Operation::AlterField { settings, .. } = parse_operation(&json!({
            "op": "alter_field", "table": "docs", "field": "code", "unique": false,
            "attributes": {"options": ["a", "b"]},
        }))
        .unwrap() else {
            panic!("an alter_field");
        };
        assert_eq!(settings.unique, Some(false));
        assert_eq!(
            settings.attributes.as_ref().and_then(|a| a.get("options")),
            Some(&json!(["a", "b"]))
        );
        let Operation::AlterField { settings, .. } = parse_operation(&json!({
            "op": "alter_field", "table": "docs", "field": "code", "label": "Code",
        }))
        .unwrap() else {
            panic!("an alter_field");
        };
        assert_eq!(settings.unique, None);
        assert_eq!(settings.attributes, None);

        let err = parse_operation(&json!({
            "op": "add_field", "table": "docs", "field": "s", "type": "string",
            "attributes": "a, b",
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("should be an object"), "{err}");
    }

    #[test]
    fn a_setting_an_operation_does_not_use_is_named_and_a_null_is_not_one() {
        let err = parse_operation(&json!({
            "op": "drop_field", "table": "docs", "field": "code", "unique": false,
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("`drop_field` does not use `unique`"), "{err}");
        assert!(err.contains("it takes `field`"), "{err}");
        let err = parse_operation(&json!({
            "op": "alter_table", "table": "docs", "fields": [], "required": true,
        }))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("`fields`") && err.contains("`required`"),
            "{err}"
        );
        let err = parse_operation(&json!({ "op": "drop_table", "table": "docs", "field": "x" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing but `table`"), "{err}");
        parse_operation(&json!({
            "op": "drop_field", "table": "docs", "field": "code", "unique": null,
        }))
        .unwrap();
        // Every setting the tool's schema offers is read by some operation.
        let params = edit_parameters();
        let offered = params["properties"][ARG_OPERATIONS]["items"]["properties"]
            .as_object()
            .unwrap();
        for key in offered.keys().filter(|k| *k != "op" && *k != "table") {
            assert!(
                OPS.iter()
                    .any(|op| op_settings(op).unwrap().contains(&key.as_str())),
                "`{key}` is offered but no operation reads it"
            );
        }
        let ops = params["properties"][ARG_OPERATIONS]["items"]["properties"]["op"]["enum"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(ops, OPS.iter().map(|o| json!(o)).collect::<Vec<_>>());
    }

    #[test]
    fn constraints_are_added_by_type_and_dropped_by_name() {
        let Operation::AddConstraint {
            given_name,
            constraint,
            ..
        } = parse_operation(&json!({
            "op": "add_constraint", "table": "docs",
            "constraint": {"type": "unique", "fields": ["document_id", "version_no"]},
        }))
        .unwrap()
        else {
            panic!("an add_constraint");
        };
        assert!(given_name.is_empty());
        assert!(constraint.name.is_empty(), "derived by the schema editor");
        assert_eq!(
            constraint.kind,
            ConstraintKind::Unique {
                fields: vec!["document_id".into(), "version_no".into()]
            }
        );
        let Operation::DropConstraint { name, .. } = parse_operation(&json!({
            "op": "drop_constraint", "table": "docs", "constraint": {"name": " sc_uq_x "},
        }))
        .unwrap() else {
            panic!("a drop_constraint");
        };
        assert_eq!(name, "sc_uq_x");
        for (item, needle) in [
            (
                json!({"op": "add_constraint", "table": "d"}),
                "needs a `constraint`",
            ),
            (
                json!({"op": "add_constraint", "table": "d", "constraint": {"type": "check"}}),
                "not a constraint type",
            ),
            (
                json!({"op": "add_constraint", "table": "d", "constraint": {"type": "unique"}}),
                "needs `fields`",
            ),
            (
                json!({"op": "drop_constraint", "table": "d", "constraint": {}}),
                "the name `describe_schema` lists",
            ),
        ] {
            let err = parse_operation(&item).unwrap_err().to_string();
            assert!(err.contains(needle), "{needle:?} not in {err}");
        }
    }

    #[test]
    fn the_tool_says_an_optional_field_may_have_a_default() {
        // The report's third point: the description said "for a required field",
        // so the agent kept `created_by` optional and set it by hand on every
        // insert. Pinned so the old sentence cannot come back.
        let params = edit_parameters();
        let item = &params["properties"][ARG_OPERATIONS]["items"]["properties"];
        for default in [
            &item["default"],
            &item["fields"]["items"]["properties"]["default"],
        ] {
            let text = default["description"].as_str().unwrap();
            assert!(text.contains("for an optional field as well"), "{text}");
            assert!(text.contains("user && user.id"), "{text}");
            assert!(!text.contains("for a required field:"), "{text}");
        }
        let text = edit_description(&Grants::all(), true);
        assert!(text.contains("works on optional fields"), "{text}");
        assert!(text.contains("add_constraint"), "{text}");
        assert!(text.contains("options"), "{text}");
    }

    #[test]
    fn a_field_unique_by_its_constraint_is_described_as_unique() {
        use sc_types::{BasicType, TypeRef};
        let mut table = crate::graphql::testing::table_of(
            "docs",
            vec![
                sc_catalog::DataField::plain("code", TypeRef::Basic(BasicType::Text)),
                sc_catalog::DataField::plain("a", TypeRef::Basic(BasicType::Int)),
                sc_catalog::DataField::plain("b", TypeRef::Basic(BasicType::Int)),
            ],
        );
        table.constraints = vec![
            TableConstraint::new(
                "docs_code_key",
                ConstraintKind::Unique {
                    fields: vec!["code".into()],
                },
            ),
            TableConstraint::new(
                "sc_uq_docs_a_b",
                ConstraintKind::Unique {
                    fields: vec!["a".into(), "b".into()],
                },
            ),
        ];
        let out = describe_table(&table, std::slice::from_ref(&table), true);
        assert_eq!(out["fields"][0]["unique"], json!(true), "{out}");
        assert_eq!(out["fields"][1]["unique"], json!(false), "{out}");
        assert_eq!(out["constraints"][1]["fields"], json!(["a", "b"]));
        assert_eq!(out["constraints"][0]["name"], json!("docs_code_key"));
    }

    #[test]
    fn a_field_cannot_be_a_reference_and_a_formula_at_once() {
        let err = parse_operation(&json!({
            "op": "add_field", "table": "t", "field": "f",
            "references": "clients", "expression": "1 + 1",
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("not both"), "{err}");
    }

    #[test]
    fn the_type_enum_in_the_schema_is_the_live_registry() {
        let params = edit_parameters();
        let types = params["properties"][ARG_OPERATIONS]["items"]["properties"]["type"]["enum"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let expected: Vec<Json> = schema_edit::field_type_names()
            .into_iter()
            .map(Json::String)
            .collect();
        assert_eq!(types, expected);
        assert!(!types.is_empty());
        // The nested `create_table` field list uses the same enum, so a type is
        // legal in one place exactly when it is legal in the other.
        assert_eq!(
            params["properties"][ARG_OPERATIONS]["items"]["properties"]["fields"]["items"]["properties"]
                ["type"]["enum"],
            Json::Array(expected)
        );
    }

    #[test]
    fn the_description_says_what_the_grants_do_not_allow() {
        let text = edit_description(&Grants::none(), true);
        assert!(text.contains("no operations at all"), "{text}");
        // No grant decides access rules (TODO-type-hardening H8.9); row-level
        // security does, and the description says so, and says not to ask for
        // it to be turned off (H8.5).
        assert!(!text.contains("allow_access_changes"), "{text}");
        assert!(text.contains("access rules are a person's"), "{text}");
        assert!(
            text.contains("Do not ask for row-level security to be turned off"),
            "{text}"
        );
        let text = edit_description(&Grants::all(), false);
        assert!(text.contains("create_table"), "{text}");
        // A database that cannot enforce RLS says so once, in the description,
        // rather than refusing it once per conversation.
        assert!(text.contains("cannot enforce row-level security"), "{text}");
    }

    #[test]
    fn the_result_tells_the_caller_which_applications_now_want_a_rebuild() {
        // §13.6's one gap: re-projection took effect at once and ran no bundler,
        // so an application served from a built bundle is now serving one
        // generated against the previous schema. The note has to say so, name
        // it, and name the tool that fixes it.
        let apps = [
            sc_catalog::ReprojectedApp {
                id: "11111111-2222-3333-4444-555555555555".to_owned(),
                subdomain: "shop".to_owned(),
                wants_build: true,
            },
            sc_catalog::ReprojectedApp {
                id: "66666666-7777-8888-9999-aaaaaaaaaaaa".to_owned(),
                subdomain: "docs".to_owned(),
                wants_build: false,
            },
        ];
        let notes = rebuild_notes(&apps).join("\n");
        assert!(notes.contains("`shop`"), "{notes}");
        assert!(notes.contains("buildApplication"), "{notes}");
        // The one with nothing to build is named too, and told so — an
        // application missing from the report reads as one that did not move.
        assert!(notes.contains("`docs`"), "{notes}");
        assert!(notes.contains("nothing to build"), "{notes}");
        // A batch that moved no application says nothing at all rather than
        // saying nothing happened.
        assert!(rebuild_notes(&[]).is_empty());
    }
}
