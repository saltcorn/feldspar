//! Type system: basic types, rich types, attributes, validation (layer 3).
//!
//! Every database column maps to a [`BasicType`] — one of a fixed set of scalar
//! families mirroring the query layer's [`Value`](sc_query::Value) variants, or
//! an [`Other`](BasicType::Other) catch-all for unrecognised backend types. On
//! top of that, a column may be given a **rich type** ([`RichType`], §6.1): one
//! Saltcorn understands, with typed attributes and validation. This crate
//! provides the pieces fields and the server need:
//!
//! - [`BasicType`] — the `Value`↔Postgres type mapping (resolve a driver
//!   `sql_type` to a value family and back to a canonical DDL type) plus
//!   value/type validation.
//! - [`RichType`] and the rich-type registry ([`registered_rich_types`],
//!   [`rich_type_config_spec`]) — a type declares its attributes as `FormField`s
//!   and validates values against them, and the admin UI renders a form for a
//!   type it knows nothing about (§6.1, [`rich`]).
//! - [`TypeRef`] — the "rich or basic" type reference a field carries, modelled
//!   as an enum so a field can be either.
//! - [`BaseField`]/[`FormField`] — the shape of a field, and a field in a form
//!   (§6.2). `FormField` is **also** how every configurable extension point
//!   declares its settings (§13.3), so the admin UI renders one form for all of
//!   them and knows about none of them.
//! - [`Attrs`] — the JSON bag those settings land in; a `FormField` describes one
//!   entry of it.
//! - [`catchall`] — the reduced display/edit path (value → text, text → value)
//!   that stands in for the full `FieldView` trait until post-MVP.
//! - [`translate_spec`] — the labels of a declared spec, translated against a
//!   request's locale before the admin API serialises them (§16.1, [`i18n`]).
//!
//! `BaseField` and `Attrs` live here rather than in `sc-catalog`, where they
//! started: neither is a catalog concept, and `FormField` needs both while
//! `sc-types` is layer 3 and cannot depend on layer 4. `DataField` stays in
//! `sc-catalog`, because its `Key`/`File` kinds reference catalog identifiers —
//! that was always the only part that had to. Both are re-exported from
//! `sc-catalog`, so `sc_catalog::{Attrs, BaseField}` still resolve.

mod attrs;
mod basic;
pub mod catchall;
mod field;
pub mod geometry;
pub mod i18n;
mod json;
mod operation;
mod rich;
mod rich_types;
mod type_ref;

pub use attrs::Attrs;
pub use basic::BasicType;
pub use field::{
    BaseField, FormField, OptionsSource, SECRET_SENTINEL, ShowIfCondition, merge_secrets,
    preserve_create_only, redact_attrs, validate_attrs,
};
pub use geometry::GeometryKind;
pub use i18n::{translate_field, translate_spec};
pub use json::{json_to_value, value_to_json};
pub use operation::{Operation, OperationScope};
pub use rich::{RichType, RichTypeRef, registered_rich_types, rich_type, rich_type_config_spec};
pub use rich_types::{IntegerType, StringType};
pub use type_ref::TypeRef;

#[cfg(test)]
mod tests {
    use super::*;
    use sc_query::Value;
    use uuid::Uuid;

    #[test]
    fn a_stable_name_round_trips_back_to_its_type() {
        // `name()` is the vocabulary a declaration carries as data — a stream's
        // element key, a module's config spec — so every variant it can produce
        // has to come back as itself.
        for ty in [
            BasicType::Bool,
            BasicType::Int,
            BasicType::Float,
            BasicType::Decimal,
            BasicType::Text,
            BasicType::Bytes,
            BasicType::Json,
            BasicType::Uuid,
            BasicType::Date,
            BasicType::Time,
            BasicType::Timestamp,
        ] {
            assert_eq!(BasicType::from_name(ty.name()), ty, "{}", ty.name());
        }
        // The SQL vocabulary still reads, so one function takes either.
        assert_eq!(BasicType::from_name("float8"), BasicType::Float);
        assert_eq!(BasicType::from_name(" INT8 "), BasicType::Int);
        // A geometry's name is its kind's.
        assert_eq!(
            BasicType::from_name("geometry_point"),
            BasicType::Geometry(GeometryKind::Point)
        );
        // And an unknown name is carried, not rejected.
        assert_eq!(
            BasicType::from_name("ltree"),
            BasicType::Other("ltree".to_owned())
        );
    }

    #[test]
    fn a_geometry_type_is_postgis_in_wgs84_and_geojson_on_the_wire() {
        use serde_json::json;
        let point = BasicType::Geometry(GeometryKind::Point);
        assert_eq!(point.sql_type(), "geometry(Point,4326)");
        assert_eq!(point.name(), "geometry_point");
        // `information_schema` says only `geometry`; `format_type` says the rest.
        assert_eq!(
            BasicType::from_sql_type("geometry"),
            BasicType::Geometry(GeometryKind::Any)
        );
        assert_eq!(BasicType::from_sql_type("geometry(Point,4326)"), point);
        // Postgres's own `point` stays what it is.
        assert_eq!(
            BasicType::from_sql_type("point"),
            BasicType::Other("point".into())
        );

        let geojson = json!({"type": "Point", "coordinates": [-0.1276, 51.5072]});
        let value = json_to_value(&point, &geojson).expect("a point");
        assert_eq!(value, Value::Json(geojson.clone()));
        assert!(point.validate(&value).is_ok());
        assert_eq!(value_to_json(&value), geojson);
        // A form sends the text of the object.
        let typed = catchall::parse(&point, &geojson.to_string()).expect("parsed");
        assert_eq!(typed, value);
        assert!(point.accepts_json(&geojson));
        // A polygon is not a point, and the refusal says so.
        let square = json!({"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 0]]]});
        let err = json_to_value(&point, &square).unwrap_err().to_string();
        assert!(err.contains("holds a point"), "{err}");
        assert!(!point.accepts(&Value::Json(square)));
        assert!(!point.accepts(&Value::Text("POINT(0 0)".into())));
    }

    #[test]
    fn maps_postgres_udt_names_to_basic_types() {
        // The aliases the driver's introspection actually emits (udt_name).
        assert_eq!(BasicType::from_sql_type("int8"), BasicType::Int);
        assert_eq!(BasicType::from_sql_type("int4"), BasicType::Int);
        assert_eq!(BasicType::from_sql_type("bool"), BasicType::Bool);
        assert_eq!(BasicType::from_sql_type("float8"), BasicType::Float);
        assert_eq!(BasicType::from_sql_type("numeric"), BasicType::Decimal);
        assert_eq!(BasicType::from_sql_type("text"), BasicType::Text);
        assert_eq!(BasicType::from_sql_type("varchar"), BasicType::Text);
        assert_eq!(BasicType::from_sql_type("bytea"), BasicType::Bytes);
        assert_eq!(BasicType::from_sql_type("jsonb"), BasicType::Json);
        assert_eq!(BasicType::from_sql_type("uuid"), BasicType::Uuid);
        assert_eq!(BasicType::from_sql_type("date"), BasicType::Date);
        assert_eq!(BasicType::from_sql_type("time"), BasicType::Time);
        assert_eq!(
            BasicType::from_sql_type("timestamptz"),
            BasicType::Timestamp
        );
    }

    #[test]
    fn resolution_is_case_insensitive_and_covers_friendly_aliases() {
        assert_eq!(BasicType::from_sql_type("BIGINT"), BasicType::Int);
        assert_eq!(
            BasicType::from_sql_type("Double Precision"),
            BasicType::Float
        );
        assert_eq!(
            BasicType::from_sql_type("timestamp with time zone"),
            BasicType::Timestamp
        );
    }

    #[test]
    fn unknown_types_become_other_and_stay_usable() {
        let t = BasicType::from_sql_type("inet");
        assert_eq!(t, BasicType::Other("inet".into()));
        // `Other` preserves its backend name for round-tripping DDL.
        assert_eq!(t.sql_type(), "inet");
        assert_eq!(t.value_kind(), None);
        // It is edited/displayed as text via the catch-all path.
        assert!(t.accepts(&Value::Text("::1".into())));
        assert!(!t.accepts(&Value::Int(1)));
    }

    #[test]
    fn canonical_sql_type_collapses_aliases() {
        // Every integer width the driver may report normalises to one DDL type.
        for udt in ["int2", "int4", "int8", "smallint", "bigint"] {
            assert_eq!(BasicType::from_sql_type(udt).sql_type(), "int8");
        }
        assert_eq!(BasicType::from_sql_type("varchar").sql_type(), "text");
        assert_eq!(BasicType::from_sql_type("json").sql_type(), "jsonb");
    }

    #[test]
    fn value_kind_agrees_with_value_kind_names() {
        // The type's expected kind must match what `Value::kind` reports, so the
        // two halves of the mapping stay in lock-step.
        let cases = [
            (BasicType::Bool, Value::Bool(true)),
            (BasicType::Int, Value::Int(1)),
            (BasicType::Float, Value::Float(1.0)),
            (BasicType::Text, Value::Text("x".into())),
            (BasicType::Uuid, Value::Uuid(Uuid::nil())),
        ];
        for (ty, val) in cases {
            assert_eq!(ty.value_kind(), Some(val.kind()));
            assert_eq!(BasicType::of_value(&val), Some(ty));
        }
    }

    #[test]
    fn null_is_accepted_by_every_type_but_has_no_type_of_its_own() {
        assert!(BasicType::Int.accepts(&Value::Null));
        assert!(BasicType::Text.accepts(&Value::Null));
        assert_eq!(BasicType::of_value(&Value::Null), None);
    }

    #[test]
    fn accepts_json_is_strict_about_the_families_json_represents() {
        use serde_json::json;

        // JSON has bools, numbers and strings, so a setting of one of those
        // families wants that shape and is not coerced from another.
        assert!(BasicType::Bool.accepts_json(&json!(true)));
        assert!(!BasicType::Bool.accepts_json(&json!("true")));
        assert!(BasicType::Int.accepts_json(&json!(4)));
        assert!(!BasicType::Int.accepts_json(&json!("4")));
        assert!(!BasicType::Int.accepts_json(&json!(1.5)));
        assert!(BasicType::Float.accepts_json(&json!(1.5)));
        assert!(BasicType::Text.accepts_json(&json!("web")));
        assert!(!BasicType::Text.accepts_json(&json!(4)));

        // A `Json` setting takes any shape at all — that is what asking for JSON
        // means.
        assert!(BasicType::Json.accepts_json(&json!({"a": [1, 2]})));
        assert!(BasicType::Json.accepts_json(&json!("anything")));

        // No basic type is a list or an object.
        assert!(!BasicType::Text.accepts_json(&json!(["a"])));
        assert!(!BasicType::Int.accepts_json(&json!({})));

        // `null` is "no value" for every type; requiredness is a separate check,
        // matching how `accepts` treats `Value::Null`.
        assert!(BasicType::Int.accepts_json(&json!(null)));
        assert!(BasicType::Text.accepts_json(&json!(null)));
    }

    #[test]
    fn accepts_json_checks_string_encoded_families_by_parsing_them() {
        use serde_json::json;

        // These have no JSON form, so they travel as strings and are checked
        // exactly the way `catchall::parse` checks a form input.
        assert!(BasicType::Uuid.accepts_json(&json!("00000000-0000-0000-0000-000000000000")));
        assert!(!BasicType::Uuid.accepts_json(&json!("not-a-uuid")));
        assert!(BasicType::Date.accepts_json(&json!("2026-07-16")));
        assert!(!BasicType::Date.accepts_json(&json!("16/07/2026")));
        assert!(BasicType::Decimal.accepts_json(&json!("1.25")));
        assert!(BasicType::Decimal.accepts_json(&json!(1.25)));
        // `Other` is edited as text, as in `accepts`.
        assert!(BasicType::Other("inet".into()).accepts_json(&json!("::1")));
        assert!(!BasicType::Other("inet".into()).accepts_json(&json!(1)));
    }

    #[test]
    fn validate_rejects_mismatched_value_families() {
        assert!(BasicType::Int.validate(&Value::Int(5)).is_ok());
        let err = BasicType::Int
            .validate(&Value::Text("nope".into()))
            .unwrap_err();
        assert!(err.to_string().contains("int"));
    }

    #[test]
    fn type_ref_delegates_to_basic() {
        let t = TypeRef::from_sql_type("int8");
        assert_eq!(t, TypeRef::Basic(BasicType::Int));
        assert_eq!(t.sql_type(), "int8");
        assert_eq!(t.name(), "int");
        assert!(t.validate(&Value::Int(3)).is_ok());
        assert!(t.validate(&Value::Bool(true)).is_err());
        assert_eq!(t.as_basic(), Some(&BasicType::Int));
    }

    #[test]
    fn catchall_round_trips_every_basic_value() {
        use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
        use rust_decimal::Decimal;

        let values = [
            (BasicType::Bool, Value::Bool(true)),
            (BasicType::Int, Value::Int(-42)),
            (BasicType::Float, Value::Float(3.5)),
            (BasicType::Decimal, Value::Decimal(Decimal::new(12345, 2))),
            (BasicType::Text, Value::Text("hello world".into())),
            (BasicType::Bytes, Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef])),
            (
                BasicType::Json,
                Value::Json(serde_json::json!({"a": 1, "b": [true, null]})),
            ),
            (
                BasicType::Uuid,
                Value::Uuid(Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0)),
            ),
            (
                BasicType::Date,
                Value::Date(NaiveDate::from_ymd_opt(2026, 7, 11).expect("valid date")),
            ),
            (
                BasicType::Time,
                Value::Time(NaiveTime::from_hms_opt(13, 30, 5).expect("valid time")),
            ),
            (
                BasicType::Timestamp,
                Value::Timestamp(
                    DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("valid ts"),
                ),
            ),
        ];
        for (ty, val) in values {
            let text = catchall::display(&val);
            let back = catchall::parse(&ty, &text).expect("parse back");
            assert_eq!(back, val, "round-trip failed for {}", ty.name());
        }
    }

    #[test]
    fn catchall_empty_input_is_null_and_null_displays_empty() {
        assert_eq!(catchall::display(&Value::Null), "");
        assert_eq!(
            catchall::parse(&BasicType::Int, "").expect("parse empty"),
            Value::Null
        );
        assert_eq!(
            catchall::parse(&BasicType::Text, "   ").expect("parse blank"),
            Value::Null
        );
    }

    #[test]
    fn catchall_reports_parse_errors() {
        let err = catchall::parse(&BasicType::Int, "not-a-number").unwrap_err();
        assert!(err.to_string().contains("int"));
        assert!(catchall::parse(&BasicType::Uuid, "xyz").is_err());
        assert!(catchall::parse(&BasicType::Json, "{bad").is_err());
    }

    #[test]
    fn catchall_accepts_common_boolean_spellings() {
        for s in ["true", "T", "1", "on", "yes"] {
            assert_eq!(
                catchall::parse(&BasicType::Bool, s).expect("parse true"),
                Value::Bool(true)
            );
        }
        for s in ["false", "f", "0", "off", "no"] {
            assert_eq!(
                catchall::parse(&BasicType::Bool, s).expect("parse false"),
                Value::Bool(false)
            );
        }
    }

    #[test]
    fn catchall_preserves_text_whitespace() {
        // Text fields keep exact whitespace; only fully-blank input is Null.
        assert_eq!(
            catchall::parse(&BasicType::Text, " padded ").expect("parse text"),
            Value::Text(" padded ".into())
        );
    }
}
