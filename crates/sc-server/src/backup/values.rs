//! How a row's values are spelt in a backup, and read back.
//!
//! The admin API's JSON ([`sc_types::value_to_json`]) is a wire shape for a
//! screen, and three of its choices are wrong for an archive:
//!
//! - **Bytes** are an array of numbers there — about 32 bytes of parsed JSON per
//!   byte of data, which is what made a 43 MB `bytea` table cost gigabytes to back
//!   up. Here they are standard padded **base64**, a third larger than the bytes
//!   and one string however long.
//! - **Non-finite floats** become `null` there, silently. Here `NaN`, `Infinity`
//!   and `-Infinity` are those strings, which the float parser the restore uses
//!   already reads.
//! - **Timestamps** are written with every sub-second digit they have, in UTC
//!   (`Z`), so a nanosecond survives the round trip.
//!
//! Everything else — a decimal as a string, an integer as an exact JSON number,
//! a UUID, a date, a time, text, JSON as itself — is spelt as the admin API spells
//! it, because that spelling already loses nothing.
//!
//! **Reading goes by the restoring table's column type**, not by the database
//! that wrote the file: a `bytea` column decodes base64 whichever backend the
//! backup came from, which is what lets a SQLite backup restore into Postgres and
//! the other way round. Every type but bytes is read by the row API's own
//! [`sc_access::rows::column_value`], so a restored value is checked exactly as a
//! value typed into a form would be.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::SecondsFormat;
use sc_catalog::Table;
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::Value;
use sc_types::BasicType;
use serde_json::{Map, Value as Json};

/// One value as a backup writes it.
pub(super) fn value_to_json(value: &Value) -> Json {
    match value {
        Value::Bytes(bytes) => Json::String(BASE64.encode(bytes)),
        Value::Float(f) if f.is_nan() => Json::String("NaN".to_owned()),
        Value::Float(f) if f.is_infinite() => Json::String(
            if f.is_sign_positive() {
                "Infinity"
            } else {
                "-Infinity"
            }
            .to_owned(),
        ),
        Value::Timestamp(ts) => Json::String(ts.to_rfc3339_opts(SecondsFormat::AutoSi, true)),
        other => sc_types::value_to_json(other),
    }
}

/// One row as a backup writes it: `{ column: value, … }`.
pub(super) fn row_to_json(row: &Row) -> Json {
    let mut map = Map::with_capacity(row.len());
    for (name, value) in row.columns().iter().zip(row.values()) {
        map.insert(name.clone(), value_to_json(value));
    }
    Json::Object(map)
}

/// The value a backup's `json` means for `column` of `table` — decoded by the
/// column's type here, and checked against it.
pub(super) fn column_value(table: &Table, column: &str, json: &Json) -> Result<Value> {
    let field = table
        .field(column)
        .ok_or_else(|| Error::invalid(format!("`{}` has no field `{column}`", table.name)))?;
    let type_ = &field.base.type_;
    let basic = match type_.as_basic() {
        Some(basic) => basic.clone(),
        None => BasicType::from_sql_type(type_.sql_type()),
    };
    if basic != BasicType::Bytes {
        return sc_access::rows::column_value(table, column, json);
    }
    let value = match json {
        Json::Null => Value::Null,
        Json::String(text) => Value::Bytes(BASE64.decode(text).map_err(|e| {
            Error::invalid(format!("`{column}`: the bytes are not valid base64: {e}"))
        })?),
        other => {
            return Err(Error::invalid(format!(
                "`{column}`: bytes are written as a base64 string, not {}",
                kind_of(other)
            )));
        }
    };
    type_
        .validate_with(&value, &field.base.attributes)
        .map_err(|e| Error::invalid(format!("`{column}`: {e}")))?;
    Ok(value)
}

fn kind_of(json: &Json) -> &'static str {
    match json {
        Json::Null => "null",
        Json::Bool(_) => "a boolean",
        Json::Number(_) => "a number",
        Json::String(_) => "a string",
        Json::Array(_) => "an array",
        Json::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
    use rust_decimal::Decimal;
    use sc_catalog::{AccessRules, DataField, DbId, TableId, TableSource};
    use sc_types::{Attrs, TypeRef};
    use std::str::FromStr;

    fn table(columns: &[(&str, BasicType)]) -> Table {
        Table {
            id: TableId("t".to_owned()),
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
