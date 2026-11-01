//! Fields, foreign keys pointing in, table constraints and the field types.

use super::*;

/// Register this domain's admin handlers on `reg`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();

    reg.register_admin("listFields", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let table = catalog.require(ctx.path_param("table")?)?;
                // Descriptions live only in the overlay row, not the merged field,
                // so they are looked up alongside — but only when the overlay
                // table exists at all (a database with no `_fd_fields` behaves as
                // before the overlay, §9).
                let metas = if catalog.get(FIELD_META_TABLE)?.is_some() {
                    list_field_meta_for_table(&catalog, &table.name).await?
                } else {
                    Vec::new()
                };
                let out: Vec<Json> = table
                    .fields
                    .iter()
                    .map(|f| field_json(f, &description_of(&metas, &f.base.name)))
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("listInboundKeys", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                // `require` first: "who points at a table that does not exist"
                // is a 404, not an empty list.
                let table = catalog.require(ctx.path_param("table")?)?;
                let projection = SchemaProjection::new(catalog.tables()?);
                let out: Vec<Json> = projection
                    .referencing_fields(&table.name)
                    .into_iter()
                    .map(|(t, f)| json!({ "table": t, "field": f }))
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("createField", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let table_name = ctx.path_param("table")?.to_owned();
                let obj = require_object(&ctx.body)?;
                let description = optional_str(obj, "description");
                // The wire shape is parsed by `field_spec_from_body`; what a field *means* — the
                // storage type behind a rich type, the foreign key behind a
                // `Key`, the calculated-field check, the DDL-then-overlay
                // sequence and the live re-projection — is the schema editor's
                // (§3.3), so an agent's `add_field` and this endpoint cannot
                // drift.
                //
                // `type` is read as optional rather than required because a
                // `Key`'s storage type is its target's, and the schema editor is
                // the one that knows that — it fills the type in for a reference
                // and names the omission for anything else. Requiring it here
                // would make the admin UI ask for an answer it cannot know and
                // the database would refuse.
                let field = field_spec_from_body(obj)?;
                let name = field.name.trim().to_owned();
                schema_edit::apply(
                    &catalog,
                    &[schema_edit::Operation::AddField {
                        table: table_name.clone(),
                        field,
                    }],
                    &schema_edit::ApplyOptions::default(),
                )
                .await?;

                let table = catalog.require(&table_name)?;
                let created = table
                    .field(&name)
                    .ok_or_else(|| Error::msg(format!("field `{name}` missing after create")))?;
                Ok(HandlerResponse::ok(field_json(created, &description)).with_status(201))
            }
        }
    });

    reg.register_admin("updateField", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                // The field must exist: this edits a field the admin is looking
                // at. The overlay, plus the two column properties an edit may
                // change: whether it is in the key, and whether it takes nulls.
                let table_name = ctx.path_param("table")?.to_owned();
                let field_name = ctx.path_param("field")?.to_owned();
                let obj = require_object(&ctx.body)?;
                let description = optional_str(obj, "description").trim().to_owned();
                // Whole-object, like `updateTable`: the admin UI edits a field it
                // has loaded, so an omitted value means cleared.
                // Omitted, like the key below, leaves the name as it is.
                let rename = obj
                    .get("name")
                    .and_then(Json::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned);
                let settings = schema_edit::FieldSettings {
                    name: rename.clone(),
                    label: Some(optional_str(obj, "label")),
                    description: Some(description.clone()),
                    type_name: Some(optional_str(obj, "type")),
                    kind: Some(parse_field_kind(obj)?),
                    attributes: Some(attributes_field(obj)?),
                    // Unlike the rest of this body, **omitted means leave it**:
                    // a form that did not ask about the key must not be able to
                    // drop one, and the field editor sends it only when the
                    // admin ticked or unticked the box.
                    primary_key: obj
                        .get("primary_key")
                        .filter(|v| !v.is_null())
                        .and_then(Json::as_bool),
                    // The same reading, for the same reason: omitted leaves the
                    // column's `NOT NULL` as it is.
                    required: obj
                        .get("required")
                        .filter(|v| !v.is_null())
                        .and_then(Json::as_bool),
                    // Back to the whole-object reading: omitted clears it.
                    default_expression: Some(optional_str(obj, "default_expression")),
                };
                schema_edit::apply(
                    &catalog,
                    &[schema_edit::Operation::AlterField {
                        table: table_name.clone(),
                        field: field_name.clone(),
                        settings,
                    }],
                    &schema_edit::ApplyOptions::default(),
                )
                .await?;
                let field_name = rename.unwrap_or(field_name);
                let table = catalog.require(&table_name)?;
                let updated = table.field(&field_name).ok_or_else(|| {
                    Error::msg(format!("field `{field_name}` missing after update"))
                })?;
                Ok(HandlerResponse::ok(field_json(updated, &description)))
            }
        }
    });

    reg.register_admin("deleteField", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                // Drops the column **and its data**, plus the overlay row that
                // described it — an overlay left behind would be
                // indistinguishable from §1.1's deliberately-kept orphan. What
                // the database would refuse with a foreign-key error is refused
                // here by name first.
                let table = ctx.path_param("table")?.to_owned();
                let field = ctx.path_param("field")?.to_owned();
                schema_edit::apply(
                    &catalog,
                    &[schema_edit::Operation::DropField {
                        table: table.clone(),
                        field: field.clone(),
                    }],
                    &schema_edit::ApplyOptions::default(),
                )
                .await?;
                Ok(HandlerResponse::ok(json!({ "dropped": field })))
            }
        }
    });

    reg.register_admin("listConstraints", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                // Read straight off the merged table, which is to say straight
                // off the database: a constraint is not stored anywhere else, so
                // there is nothing here to look up beside it (§5).
                let table = catalog.require(ctx.path_param("table")?)?;
                let out: Vec<Json> = table.constraints.iter().map(constraint_json).collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("createConstraint", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let table_name = ctx.path_param("table")?.to_owned();
                let obj = require_object(&ctx.body)?;
                let (given_name, constraint) = constraint_from_body(obj)?;
                // Through the schema editor like every other schema change, so
                // an agent adding a constraint and this endpoint cannot drift —
                // and so the DDL joins one transaction with anything else in the
                // batch (§3.3).
                let name =
                    TableConstraint::derived_name(&table_name, &constraint.kind, &given_name);
                schema_edit::apply(
                    &catalog,
                    &[schema_edit::Operation::AddConstraint {
                        table: table_name.clone(),
                        given_name,
                        constraint,
                    }],
                    &schema_edit::ApplyOptions::default(),
                )
                .await?;
                let table = catalog.require(&table_name)?;
                let created = table
                    .constraints
                    .iter()
                    .find(|c| c.name == name)
                    .ok_or_else(|| {
                        Error::msg(format!("constraint `{name}` missing after create"))
                    })?;
                Ok(HandlerResponse::ok(constraint_json(created)).with_status(201))
            }
        }
    });

    reg.register_admin("deleteConstraint", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let table = ctx.path_param("table")?.to_owned();
                let name = ctx.path_param("constraint")?.to_owned();
                schema_edit::apply(
                    &catalog,
                    &[schema_edit::Operation::DropConstraint {
                        table,
                        name: name.clone(),
                    }],
                    &schema_edit::ApplyOptions::default(),
                )
                .await?;
                Ok(HandlerResponse::ok(json!({ "dropped": name })))
            }
        }
    });

    reg.register_admin("listFieldTypes", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let mut out = Vec::new();
                // Basic types first (no attributes), then rich types with their
                // attribute specs, then the Key/File kinds — one list the field
                // editor assembles its picker from (§3.4).
                for basic in schema_edit::basic_field_types() {
                    // A vector's dimensions and embedding are attributes, and
                    // the only basic type that has any (TODO-agent-skills AS2).
                    let spec = match basic {
                        sc_types::BasicType::Vector(_) => sc_types::vector::vector_attributes(),
                        _ => Vec::new(),
                    };
                    out.push(field_type_json(basic.name(), "basic", &spec, &ctx.locale));
                }
                for name in registered_rich_types() {
                    let spec = rich_type_config_spec(&name)?;
                    out.push(field_type_json(&name, "rich", &spec, &ctx.locale));
                }
                for (name, spec) in [
                    ("key", key_kind_config_spec()),
                    ("file", file_kind_config_spec()),
                    // A secret takes no parameters (TODO-imap M2).
                    ("secret", Vec::new()),
                ] {
                    // Resolve any `server_query` (the File store pick-list) to a
                    // static list before the spec leaves the server.
                    let resolved = resolve_options(&catalog, spec).await?;
                    out.push(field_type_json(name, "kind", &resolved, &ctx.locale));
                }
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });
}

/// Read a `createConstraint` body into the short name an admin gave (only a row
/// constraint has one) and the constraint itself.
///
/// The `type` decides which of the body's fields are read, and a missing one is
/// named rather than defaulted: a unique constraint with no fields would be a
/// constraint over nothing, and a row constraint with no formula a trigger that
/// checks nothing.
fn constraint_from_body(obj: &Map<String, Json>) -> Result<(String, TableConstraint)> {
    let type_name = obj.non_empty_str_field("type")?.trim().to_owned();
    let kind = match type_name.as_str() {
        "unique" => ConstraintKind::Unique {
            fields: string_array(obj, "fields")?,
        },
        "index" => ConstraintKind::Index {
            fields: string_array(obj, "fields")?,
            expression: None,
            method: "btree".to_owned(),
        },
        "full_text_search" => ConstraintKind::FullTextSearch {
            // The default is the ordinary one: an installation that has not
            // thought about stemming wants English rather than an error.
            language: match optional_str(obj, "language").trim() {
                "" => "english".to_owned(),
                given => given.to_owned(),
            },
        },
        "formula" => ConstraintKind::Formula {
            formula: obj.non_empty_str_field("formula")?.trim().to_owned(),
        },
        "vector_index" => {
            let fields = string_array(obj, "fields")?;
            let [field] = fields.as_slice() else {
                return Err(Error::invalid(
                    "a vector index is over one vector field: give `fields` as [\"<field>\"]",
                ));
            };
            ConstraintKind::VectorIndex {
                field: field.clone(),
            }
        }
        other => {
            return Err(Error::invalid(format!(
                "`{other}` is not a constraint type; it is one of `unique`, `index`, \
                 `full_text_search`, `vector_index` or `formula`"
            )));
        }
    };
    let mut constraint = TableConstraint::new(String::new(), kind);
    constraint.error_message = Some(optional_str(obj, "error_message"))
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty());
    Ok((optional_str(obj, "name").trim().to_owned(), constraint))
}

/// A required array-of-strings field, e.g. the fields of a unique constraint.
fn string_array(obj: &Map<String, Json>, key: &str) -> Result<Vec<String>> {
    let array = obj
        .get(key)
        .and_then(Json::as_array)
        .ok_or_else(|| Error::invalid(format!("`{key}` must be an array of field names")))?;
    array
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::invalid(format!("`{key}` must be an array of field names")))
        })
        .collect()
}

/// One `listFieldTypes` entry.
fn field_type_json(name: &str, category: &str, spec: &[FormField], locale: &Locale) -> Json {
    json!({
        "name": name,
        "label": name,
        "category": category,
        "config_spec": spec_json(spec, locale),
    })
}

/// The overlay description for a field, from the metas loaded for its table.
fn description_of(metas: &[FieldMeta], field: &str) -> String {
    metas
        .iter()
        .find(|m| m.field_name == field)
        .map(|m| m.description.clone())
        .unwrap_or_default()
}
