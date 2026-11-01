//! The admin API's endpoints: fields, foreign keys pointing in, table constraints and the field
//! types.
//!
//! `sc-server`'s `handlers::fields` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
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

    // Overlay edits: label, description, rich type, kind parameters,
    // attributes; plus the key, `required`, and `name`, which renames the
    // column (its row history follows, §14.12). Retyping a column is a
    // migration and is out of scope (§3.3), so this endpoint cannot touch
    // `unique` or the storage SQL type.
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
        // The JavaScript expression an insert that leaves the field out takes
        // its value from, with `user` and the row in scope; empty for none.
        StructField::new("default_expression", TypeSchema::text()),
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
        // Only on a required field (`schema_edit::FieldSpec::default_expression`).
        StructField::new(
            "default_expression",
            TypeSchema::optional(TypeSchema::text()),
        ),
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
        // Omitted keeps the name; another renames the field.
        StructField::new("name", TypeSchema::optional(TypeSchema::text())),
        StructField::new("type", TypeSchema::optional(TypeSchema::text())),
        StructField::new("kind", TypeSchema::optional(TypeSchema::json())),
        StructField::new("attributes", TypeSchema::optional(TypeSchema::json())),
        StructField::new("label", TypeSchema::optional(TypeSchema::text())),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("primary_key", TypeSchema::optional(TypeSchema::bool())),
        StructField::new("required", TypeSchema::optional(TypeSchema::bool())),
        // Whole-object like the rest: omitted or empty clears it.
        StructField::new(
            "default_expression",
            TypeSchema::optional(TypeSchema::text()),
        ),
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
