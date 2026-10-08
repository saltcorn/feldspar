//! What a column looks like on the GraphQL wire.
//!
//! GraphQL's built-in scalars are `Int`, `Float`, `String`, `Boolean` and `ID`,
//! and a database's type system is larger than that. Every mapping here is
//! decided by one rule: **a scalar must not lose information silently.**
//!
//! - GraphQL's `Int` is fixed by the specification at **32 bits**. Our
//!   [`ValueType::Int`] is 64-bit, so it maps to a custom
//!   [`BIG_INT`](super::names::BIG_INT) scalar rather than to `Int`. Mapping it
//!   to `Int` would work until an id got large; mapping it to `Float` would
//!   round it. Both are failures nobody sees until the data is already wrong.
//! - A decimal maps to [`DECIMAL`](super::names::DECIMAL), carried as a string,
//!   for the reason the column is a decimal in the first place: a JSON number is
//!   an IEEE double.
//! - Dates, times, timestamps, UUIDs, bytes and embedded JSON get named scalars
//!   too. A client that knows what `Date` means can do better than one handed a
//!   `String`, and the SDL is where it finds out.
//!
//! Two field kinds are not scalars at all:
//!
//! - A **`Key`** field projects as the *target table's object type*, because
//!   that is what a caller wants from a foreign key — `manager { email }`, one
//!   correlated subquery, no second round trip. When the target table is not
//!   part of this application's schema the field falls back to the scalar the
//!   column actually holds, which is still real data.
//! - A **`File`** field projects as [`FILE_VALUE`](super::names::FILE_VALUE):
//!   the stored path, plus the URL the **REST** provider already serves the
//!   bytes at. A GraphQL field must not become a second file-download path —
//!   the file's own access rules are enforced in one place, and a second door
//!   into the bytes is a second place to get them wrong.

use async_graphql::dynamic::TypeRef;
use sc_catalog::{DataField, DataFieldKind};

use super::names::{
    BIG_INT, BYTES, DATE, DECIMAL, FILE_VALUE, GEO_JSON, JSON, SchemaNames, TIME, TIMESTAMP, UUID,
};
use crate::schema::ValueType;

/// The custom scalars the schema defines, in SDL order.
pub const CUSTOM_SCALARS: &[&str] = &[
    BIG_INT, BYTES, DATE, DECIMAL, GEO_JSON, JSON, TIME, TIMESTAMP, UUID,
];

/// The GraphQL scalar name a wire [`ValueType`] is carried as.
pub fn scalar_name(ty: ValueType) -> &'static str {
    match ty {
        ValueType::Bool => TypeRef::BOOLEAN,
        ValueType::Float => TypeRef::FLOAT,
        ValueType::Text => TypeRef::STRING,
        ValueType::Int => BIG_INT,
        ValueType::Decimal => DECIMAL,
        ValueType::Bytes => BYTES,
        ValueType::Json => JSON,
        ValueType::Uuid => UUID,
        ValueType::Date => DATE,
        ValueType::Time => TIME,
        ValueType::Timestamp => TIMESTAMP,
        ValueType::Geometry => GEO_JSON,
    }
}

/// The scalar a column is carried as, ignoring what it *references*.
///
/// A rich type has no separate wire form — it is a basic type with rules — so it
/// maps through the basic type it is stored as, exactly as the REST projection
/// does. A rich type whose `TypeRef` is not basic is carried as text, the same
/// fallback [`ValueType::from_basic`] gives [`BasicType::Other`].
///
/// [`BasicType::Other`]: sc_types::BasicType::Other
pub fn column_scalar(field: &DataField) -> &'static str {
    let ty = field
        .base
        .type_
        .as_basic()
        .map_or(ValueType::Text, ValueType::from_basic);
    scalar_name(ty)
}

/// The GraphQL type a row field projects as, including its nullability.
///
/// A `NOT NULL` column is non-null on the wire; everything else is nullable,
/// including a `Key` whose foreign key may be null (an absent relation is
/// `null`, which is the Ⱶ operator's own contract) and every calculated field
/// (an expression's value is not constrained by a column's `NOT NULL`).
pub fn field_type(field: &DataField, names: &SchemaNames) -> TypeRef {
    let named = |name: &str| {
        if field.required {
            TypeRef::named_nn(name)
        } else {
            TypeRef::named(name)
        }
    };
    match &field.kind {
        // A key projects as the object it points at — but only when this
        // application exposes that table. When it does not, the column's own
        // value is still real data and is served as such.
        DataFieldKind::Key { target_table, .. } => match names.get(&target_table.0) {
            // Never non-null: even a `NOT NULL` foreign key resolves to `null`
            // when the caller may not read the referenced row, and a non-null
            // field that resolves to null is a GraphQL error that would take
            // the whole parent with it.
            Some(target) => TypeRef::named(&target.object),
            None => named(column_scalar(field)),
        },
        DataFieldKind::File { .. } => named(FILE_VALUE),
        // A calculated field has no column behind it, so `required` is whatever
        // the overlay happened to say; it is nullable on the wire regardless.
        DataFieldKind::Calc { .. } => TypeRef::named(column_scalar(field)),
        DataFieldKind::Plain => named(column_scalar(field)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphql::names::SchemaNames;
    use crate::graphql::testing::{file_field, id_field, key_field, table_of, typed_field};
    use sc_types::BasicType;

    #[test]
    fn a_64_bit_integer_is_not_graphqls_32_bit_int() {
        assert_eq!(scalar_name(ValueType::Int), "BigInt");
        assert_ne!(scalar_name(ValueType::Int), TypeRef::INT);
        // …and a decimal is not a Float, for the same reason in the other
        // direction: the column exists to be exact.
        assert_eq!(scalar_name(ValueType::Decimal), "Decimal");
        assert_ne!(scalar_name(ValueType::Decimal), TypeRef::FLOAT);
    }

    #[test]
    fn the_scalars_graphql_does_have_are_the_ones_used() {
        assert_eq!(scalar_name(ValueType::Bool), "Boolean");
        assert_eq!(scalar_name(ValueType::Text), "String");
        assert_eq!(scalar_name(ValueType::Float), "Float");
    }

    #[test]
    fn required_columns_are_non_null_and_the_rest_are_not() {
        let names = SchemaNames::derive(&[table_of("t", vec![id_field()])]);
        let required = id_field();
        assert_eq!(field_type(&required, &names).to_string(), "BigInt!");
        let optional = typed_field("note", BasicType::Text);
        assert_eq!(field_type(&optional, &names).to_string(), "String");
    }

    #[test]
    fn a_key_projects_as_the_target_type_when_the_app_exposes_it() {
        let names = SchemaNames::derive(&[
            table_of("users", vec![id_field()]),
            table_of(
                "departments",
                vec![id_field(), key_field("manager", "users", "id")],
            ),
        ]);
        let manager = key_field("manager", "users", "id");
        assert_eq!(field_type(&manager, &names).to_string(), "Users");

        // A key to a table this application does not declare has no object type
        // to point at, so it carries the column's own value instead.
        let outside = SchemaNames::derive(&[table_of("departments", vec![id_field()])]);
        let manager = key_field("manager", "users", "id");
        assert_eq!(field_type(&manager, &outside).to_string(), "BigInt");
    }

    #[test]
    fn a_required_key_is_still_nullable_on_the_wire() {
        // A row the caller may not read resolves to null; a non-null field that
        // resolves to null would propagate the error up to the parent list.
        let names = SchemaNames::derive(&[
            table_of("users", vec![id_field()]),
            table_of("departments", vec![id_field()]),
        ]);
        let mut manager = key_field("manager", "users", "id");
        manager.required = true;
        assert_eq!(field_type(&manager, &names).to_string(), "Users");
    }

    #[test]
    fn a_file_field_is_a_path_and_a_url_not_a_download() {
        let names = SchemaNames::derive(&[table_of("t", vec![id_field()])]);
        let avatar = file_field("avatar", "uploads");
        assert_eq!(field_type(&avatar, &names).to_string(), "FileValue");
    }
}
