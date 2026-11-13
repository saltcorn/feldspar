//! How a row's values are spelt in a backup, and read back.
//!
//! The spelling is [`sc_types::Spelling::Archive`], kept beside the other three
//! in `sc_types::json`, whose table says how it differs from the admin API's
//! JSON: bytes in base64, non-finite floats named, timestamps with every
//! sub-second digit in UTC.
//!
//! **Reading goes by the restoring table's column type**, not by the database
//! that wrote the file: a `bytea` column decodes base64 whichever backend the
//! backup came from, which is what lets a SQLite backup restore into Postgres and
//! the other way round. Every type but bytes is read by the row API's own
//! [`sc_access::rows::column_value`], so a restored value is checked exactly as a
//! value typed into a form would be.

use sc_catalog::Table;
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::Value;
use sc_types::{BasicType, Spelling, json_to_value_as, value_to_json_as};
use serde_json::{Map, Value as Json};

/// One value as a backup writes it.
pub(super) fn value_to_json(value: &Value) -> Json {
    value_to_json_as(value, Spelling::Archive)
}

/// One row as a backup writes it: `{ column: value, … }`.
pub(super) fn row_to_json(row: &Row) -> Json {
    let mut map = Map::with_capacity(row.len());
    for (name, value) in row.columns().iter().zip(row.values()) {
        map.insert(name.clone(), value_to_json(value));
    }
    Json::Object(map)
}

/// How a column's values are packed in a backup ([`sc_types::vector_pack`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Packing {
    None,
    Vector,
    HalfVector,
    Sparse,
}

impl Packing {
    fn of(basic: &BasicType) -> Packing {
        match basic {
            BasicType::Vector(_) => Packing::Vector,
            BasicType::HalfVector(_) => Packing::HalfVector,
            BasicType::Other(name) if is_sparse(name) => Packing::Sparse,
            _ => Packing::None,
        }
    }

    fn of_column(table: &Table, column: &str) -> Packing {
        table
            .field(column)
            .map_or(Packing::None, |f| Packing::of(&storage_type(&f.base.type_)))
    }

    /// `value` as this packing spells it, or as [`value_to_json`] does when it
    /// is not a value of the kind (a null, or one that will not pack).
    fn spell(self, value: &Value) -> Json {
        let packed = match (self, value) {
            (Packing::Vector, Value::Json(json)) => sc_types::vector_pack::pack(json, false),
            (Packing::HalfVector, Value::Json(json)) => sc_types::vector_pack::pack(json, true),
            (Packing::Sparse, Value::Text(text)) => sc_types::vector_pack::pack_sparse(text),
            _ => None,
        };
        packed.map_or_else(|| value_to_json(value), Json::String)
    }
}

/// pgvector's `sparsevec`, which this system reads as text (`BasicType::Other`).
fn is_sparse(sql_type: &str) -> bool {
    let name = sql_type.trim().to_ascii_lowercase();
    name == "sparsevec" || name.starts_with("sparsevec(")
}

fn storage_type(type_: &sc_types::TypeRef) -> BasicType {
    match type_.as_basic() {
        Some(basic) => basic.clone(),
        None => BasicType::from_sql_type(type_.sql_type()),
    }
}

/// How each column of `table`'s rows is packed, in the order `columns` names
/// them — worked out once for a page of rows, not once a value.
pub(super) fn packings(table: &Table, columns: &[String]) -> Vec<Packing> {
    columns
        .iter()
        .map(|c| Packing::of_column(table, c))
        .collect()
}

/// One row as a backup writes it: [`row_to_json`], with its vector columns
/// packed as [`packings`] says ([`sc_types::vector_pack`]) — the bulk of a table
/// that has them, at a fifth of the size of their decimal text.
pub(super) fn packed_row_to_json(packings: &[Packing], row: &Row) -> Json {
    let mut map = Map::with_capacity(row.len());
    for (i, (name, value)) in row.columns().iter().zip(row.values()).enumerate() {
        let packing = packings.get(i).copied().unwrap_or(Packing::None);
        map.insert(name.clone(), packing.spell(value));
    }
    Json::Object(map)
}

/// A packed value ([`packed_row_to_json`]) of a column of `packing` as the JSON
/// the column's type reads; any other JSON as it is — an older backup's array
/// of numbers or `sparsevec` text, or a null.
fn unpacked(packing: Packing, column: &str, json: &Json) -> Result<Option<Json>> {
    let Json::String(text) = json else {
        return Ok(None);
    };
    let found = match packing {
        Packing::Vector | Packing::HalfVector => sc_types::vector_pack::unpack(text),
        Packing::Sparse => sc_types::vector_pack::unpack_sparse(text).map(|r| r.map(Json::String)),
        Packing::None => None,
    };
    found
        .transpose()
        .map_err(|e| Error::invalid(format!("`{column}`: {}", e.message())))
}

/// The value a backup's `json` means for `column` of `table` — decoded by the
/// column's type here, and checked against it.
pub(super) fn column_value(table: &Table, column: &str, json: &Json) -> Result<Value> {
    let field = table
        .field(column)
        .ok_or_else(|| Error::invalid(format!("`{}` has no field `{column}`", table.name)))?;
    let type_ = &field.base.type_;
    let basic = storage_type(type_);
    if let Some(json) = unpacked(Packing::of(&basic), column, json)? {
        return sc_access::rows::column_value(table, column, &json);
    }
    if basic != BasicType::Bytes {
        return sc_access::rows::column_value(table, column, json);
    }
    let value = json_to_value_as(&basic, json, Spelling::Archive)
        .map_err(|e| Error::invalid(format!("`{column}`: {}", e.message())))?;
    type_
        .validate_with(&value, &field.base.attributes)
        .map_err(|e| Error::invalid(format!("`{column}`: {e}")))?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
    use rust_decimal::Decimal;
    use sc_catalog::{AccessRules, DataField, DbId, TableSource};
    use sc_types::{Attrs, TypeRef};
    use std::str::FromStr;

    fn table(columns: &[(&str, BasicType)]) -> Table {
        Table {
            name: "t".to_owned(),
            database: DbId::primary(),
            source: TableSource::Database,
            primary_key: Vec::new(),
            fields: columns
                .iter()
                .map(|(name, basic)| DataField::plain(*name, TypeRef::Basic(basic.clone())))
                .collect(),
            label: "t".to_owned(),
            description: String::new(),
            access: AccessRules::default(),
            attributes: Attrs::new(),
            overlay: None,
            ownership: None,
            ownership_error: None,
            rls_enabled: false,
            constraints: Vec::new(),
        }
    }

    /// Through the writer's spelling and back by the column's type — and
    /// through the *text* of a line, since that is what the file holds.
    fn round_trip(basic: BasicType, value: Value) -> Value {
        let t = table(&[("c", basic)]);
        let line = serde_json::to_string(&value_to_json(&value)).unwrap();
        let json: Json = serde_json::from_str(&line).unwrap();
        column_value(&t, "c", &json).unwrap()
    }

    /// A row of `t` through the packed writer, the text of its line, and back.
    fn packed_round_trip(t: &Table, row: &Row) -> (Json, Vec<Value>) {
        let line =
            serde_json::to_string(&packed_row_to_json(&packings(t, row.columns()), row)).unwrap();
        let json: Json = serde_json::from_str(&line).unwrap();
        let values = row
            .columns()
            .iter()
            .map(|c| column_value(t, c, &json[c]).unwrap())
            .collect();
        (json, values)
    }

    #[test]
    fn vectors_are_packed_and_read_back_whichever_spelling_the_file_has() {
        use serde_json::json;
        let t = table(&[
            ("v", BasicType::Vector(3)),
            ("h", BasicType::HalfVector(3)),
            ("s", BasicType::Other("sparsevec(5)".into())),
            ("note", BasicType::Text),
        ]);
        let columns = std::sync::Arc::new(vec![
            "v".to_owned(),
            "h".to_owned(),
            "s".to_owned(),
            "note".to_owned(),
        ]);
        let original = vec![
            Value::Json(json!([0.1, -2.5, 3.0])),
            Value::Json(json!([0.099975586, 1.0, -2.0])),
            Value::Text("{1:1.5,4:2}/5".into()),
            // Text that looks packed stays text in a text column.
            Value::Text("f32le:AAAAAA==".into()),
        ];
        let row = Row::new(columns.clone(), original.clone()).unwrap();
        let (json, values) = packed_round_trip(&t, &row);
        assert!(json["v"].as_str().unwrap().starts_with("f32le:"), "{json}");
        assert!(json["h"].as_str().unwrap().starts_with("f16le:"), "{json}");
        assert!(
            json["s"].as_str().unwrap().starts_with("sparse:5:"),
            "{json}"
        );
        assert_eq!(json["note"], json!("f32le:AAAAAA=="));
        assert_eq!(values, original);

        // Nulls stay null.
        let row = Row::new(columns, vec![Value::Null; 4]).unwrap();
        let (json, values) = packed_round_trip(&t, &row);
        assert_eq!(json["v"], Json::Null);
        assert_eq!(values, vec![Value::Null; 4]);

        // A backup written before packing: arrays and the sparse text.
        assert_eq!(
            column_value(&t, "v", &json!([0.1, -2.5, 3.0])).unwrap(),
            original[0]
        );
        assert_eq!(
            column_value(&t, "h", &json!([0.099975586, 1.0, -2.0])).unwrap(),
            original[1]
        );
        assert_eq!(
            column_value(&t, "s", &json!("{1:1.5,4:2}/5")).unwrap(),
            original[2]
        );
        // A damaged packed value is refused, naming the column.
        let err = column_value(&t, "v", &json!("f32le:!!")).unwrap_err();
        assert!(err.to_string().contains("`v`"), "{err}");
    }

    #[test]
    fn every_kind_of_value_survives_the_round_trip() {
        let uuid = uuid::Uuid::new_v4();
        let cases = vec![
            (BasicType::Bool, Value::Bool(true)),
            (BasicType::Int, Value::Int(i64::MAX)),
            (BasicType::Int, Value::Int(i64::MIN)),
            (BasicType::Float, Value::Float(0.1)),
            (BasicType::Float, Value::Float(f64::MIN_POSITIVE)),
            (BasicType::Float, Value::Float(-1.234_567_890_123_456_7e300)),
            (BasicType::Float, Value::Float(f64::INFINITY)),
            (BasicType::Float, Value::Float(f64::NEG_INFINITY)),
            (
                BasicType::Decimal,
                Value::Decimal(Decimal::from_str("1234567890.123456789012345678").unwrap()),
            ),
            (BasicType::Text, Value::Text("ÆØÅ — 日本語 🙂".to_owned())),
            (BasicType::Bytes, Value::Bytes(Vec::new())),
            (BasicType::Bytes, Value::Bytes((0..=255).collect())),
            (
                BasicType::Json,
                Value::Json(serde_json::json!({"a": [1, null, "x"]})),
            ),
            (BasicType::Uuid, Value::Uuid(uuid)),
            (
                BasicType::Date,
                Value::Date(NaiveDate::from_ymd_opt(1066, 10, 14).unwrap()),
            ),
            (
                BasicType::Time,
                Value::Time(NaiveTime::from_hms_micro_opt(23, 59, 58, 123_456).unwrap()),
            ),
            (
                BasicType::Timestamp,
                Value::Timestamp(
                    Utc.with_ymd_and_hms(2026, 10, 19, 12, 34, 56).unwrap()
                        + chrono::Duration::nanoseconds(123_456_789),
                ),
            ),
            (BasicType::Int, Value::Null),
            (BasicType::Bytes, Value::Null),
        ];
        for (basic, value) in cases {
            assert_eq!(round_trip(basic.clone(), value.clone()), value, "{basic:?}");
        }
        // NaN is not equal to itself, so it is checked by what it is.
        match round_trip(BasicType::Float, Value::Float(f64::NAN)) {
            Value::Float(f) => assert!(f.is_nan()),
            other => panic!("NaN came back as {other:?}"),
        }
    }

    #[test]
    fn bytes_are_base64_and_floats_that_are_not_numbers_are_named() {
        assert_eq!(
            value_to_json(&Value::Bytes(b"hello".to_vec())),
            Json::String("aGVsbG8=".to_owned())
        );
        assert_eq!(value_to_json(&Value::Float(f64::NAN)), Json::from("NaN"));
        assert_eq!(
            value_to_json(&Value::Float(f64::NEG_INFINITY)),
            Json::from("-Infinity")
        );
        assert_eq!(
            value_to_json(&Value::Timestamp(
                Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
                    + chrono::Duration::nanoseconds(7)
            )),
            Json::from("2026-01-02T03:04:05.000000007Z")
        );
    }

    #[test]
    fn bytes_that_are_not_base64_are_refused() {
        let t = table(&[("c", BasicType::Bytes)]);
        for bad in [
            Json::from("not base64!"),
            serde_json::json!([1, 2, 3]),
            Json::from(7),
        ] {
            let e = column_value(&t, "c", &bad).unwrap_err();
            assert!(e.to_string().contains("`c`"), "{e}");
        }
    }
}
