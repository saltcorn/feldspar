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
    ATTR_OWNERSHIP_FORMULA, Catalog, DataFieldKind, FieldId, FileStoreId, Table, TableId,
};
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::{Map, Value as Json, json};

use super::{AdminTool, ToolContext, optional_bool, optional_role, optional_string};

/// The reading tool's name. Fixed rather than derived, because this surface is
/// configured against no table to derive one from — which is also what makes a
/// second `admin_copilot` on one agent refusable on save (§11.2): the two
/// instances offer the same names, and the collision check refuses that where
/// it is fixable rather than leaving the model to pick between duplicates.
pub const TOOL_DESCRIBE: &str = "describe_schema";
/// The writing tool's name.
pub const TOOL_EDIT: &str = "edit_schema";

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
        edit_description(grants, catalog.primary().capabilities().row_level_security)
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
    let rls_available = catalog.primary().capabilities().row_level_security;
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
    let fields: Vec<Json> = table.fields.iter().map(describe_field).collect();
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
                "target_table": target_table.0,
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
                } if target_table.0 == table.name => Some(json!({
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
        "fields": fields,
        "references": references,
        "referenced_by": referenced_by,
    })
}

fn describe_field(field: &sc_catalog::DataField) -> Json {
    let mut out = Map::new();
    out.insert("name".to_owned(), json!(field.base.name));
    out.insert("label".to_owned(), json!(field.base.label));
    out.insert("type".to_owned(), json!(field.base.type_.name()));
    out.insert(
        "storage_type".to_owned(),
        json!(field.base.type_.sql_type()),
    );
    out.insert("required".to_owned(), json!(field.required));
    out.insert("unique".to_owned(), json!(field.unique));
    out.insert("primary_key".to_owned(), json!(field.primary_key));
    match &field.kind {
        DataFieldKind::Key {
            target_table,
            target_field,
            summary_field,
        } => {
            out.insert("references".to_owned(), json!(target_table.0));
            out.insert("references_field".to_owned(), json!(target_field.0));
            if let Some(summary) = summary_field {
                out.insert("summary_field".to_owned(), json!(summary.0));
            }
        }
        DataFieldKind::Calc { expression } => {
            out.insert("calculated".to_owned(), json!(expression));
        }
        DataFieldKind::File { store, .. } => {
            out.insert("file_store".to_owned(), json!(store.0));
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

fn edit_description(grants: &Grants, rls_available: bool) -> String {
    let mut allowed: Vec<&str> = Vec::new();
    if grants.create {
        allowed.push("create_table");
    }
    if grants.edit {
        allowed.push("alter_table, add_field, alter_field");
    }
    if grants.drop {
        allowed.push("drop_field, drop_table");
    }
    let permitted = match allowed.is_empty() {
        true => "You are permitted no operations at all; this tool will refuse \
                 every batch. Say so rather than retrying."
            .to_owned(),
        false => format!("You are permitted: {}.", allowed.join(", ")),
    };
    let access = match grants.access_changes {
        true => "You may also set a table's access rules (`min_role_read`, \
                 `min_role_write`, `ownership_formula`, `rls_enabled`). These \
                 change what every other user of this deployment can reach, so \
                 say plainly what you are about to do before you do it."
            .to_owned(),
        false => "You may **not** set access rules; a batch naming \
                  `min_role_read`, `min_role_write`, `ownership_formula` or \
                  `rls_enabled` is refused whole."
            .to_owned(),
    };
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
                                "create_table", "alter_table", "add_field",
                                "alter_field", "drop_field", "drop_table",
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
                            "description": "The column carries a unique constraint.",
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
    parameters
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
            parse_operation(item).map_err(|e| Error::invalid(format!("operation {index}: {e}")))?,
        );
    }

    check_file_stores(catalog, &operations)?;

    let applied = schema_edit::apply(
        catalog,
        &operations,
        &ApplyOptions {
            grants: *grants,
            dry_run,
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
        "fields_added": applied.fields_added,
        "fields_altered": applied.fields_altered,
        "fields_dropped": applied.fields_dropped,
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
        other => Err(Error::invalid(format!(
            "unknown `op` `{other}`; it is one of create_table, alter_table, \
             add_field, alter_field, drop_field, drop_table"
        ))),
    }
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
            && catalog.file_store(&store.0)?.is_none()
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
        label: optional_string(obj, "label")?,
        description: optional_string(obj, "description")?,
        min_role_read: optional_role(obj, "min_role_read")?,
        min_role_write: optional_role(obj, "min_role_write")?,
        ownership_formula: optional_string(obj, "ownership_formula")?,
        rls_enabled: optional_bool(obj, "rls_enabled")?,
    })
}

fn parse_field_settings(obj: &Map<String, Json>) -> Result<FieldSettings> {
    let kind = field_kind(obj)?;
    Ok(FieldSettings {
        label: optional_string(obj, "label")?,
        description: optional_string(obj, "description")?,
        type_name: optional_string(obj, "type")?,
        kind,
        attributes: None,
        primary_key: optional_bool(obj, "primary_key")?,
        required: optional_bool(obj, "required")?,
    })
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
        type_name: optional_string(obj, "type")?.unwrap_or_default(),
        label: optional_string(obj, "label")?.unwrap_or_default(),
        description: optional_string(obj, "description")?.unwrap_or_default(),
        required: optional_bool(obj, "required")?.unwrap_or(false),
        unique: optional_bool(obj, "unique")?.unwrap_or(false),
        primary_key: optional_bool(obj, "primary_key")?.unwrap_or(false),
        kind: field_kind(obj)?.unwrap_or(DataFieldKind::Plain),
        attributes: Attrs::new(),
    })
}

/// A field's kind from the flat item: `references` makes it a foreign key,
/// `expression` a calculated field and `file_store` a file field, and more than
/// one of them is a contradiction worth naming rather than resolving by
/// precedence.
fn field_kind(obj: &Map<String, Json>) -> Result<Option<DataFieldKind>> {
    let references = optional_string(obj, "references")?.filter(|s| !s.trim().is_empty());
    let expression = optional_string(obj, "expression")?.filter(|s| !s.trim().is_empty());
    let file_store = optional_string(obj, ARG_FILE_STORE)?.filter(|s| !s.trim().is_empty());
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
            folder: optional_string(obj, ARG_FILE_FOLDER)?
                .map(|f| f.trim().trim_matches('/').to_owned())
                .filter(|f| !f.is_empty()),
            mime_allow,
        }));
    }
    match (references, expression) {
        (Some(_), Some(_)) => Err(Error::invalid(
            "a field is either a reference or a calculated expression, not both",
        )),
        (Some(target), None) => Ok(Some(DataFieldKind::Key {
            target_table: TableId(target.trim().to_owned()),
            // Empty: the schema editor resolves it to the target's primary key,
            // so the model never has to know (or guess) the column's name.
            target_field: FieldId(String::new()),
            summary_field: optional_string(obj, "summary_field")?
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
                assert_eq!(target_table.0, "clients");
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
        assert_eq!(settings.min_role_read, Some(40));
        // The three settings the caller did not name are `None` — "leave it" —
        // which is the whole divergence from `updateTable`'s whole-object
        // contract (§13.1).
        assert_eq!(settings.min_role_write, None);
        assert_eq!(settings.ownership_formula, None);
        assert_eq!(settings.rls_enabled, None);
        assert_eq!(settings.label, None);
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
        assert!(text.contains("may **not** set access rules"), "{text}");
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
