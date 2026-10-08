//! Converting between JSON (the wire shape of the typed API) and the query
//! layer's [`Value`] (technical design §13.2, §4).
//!
//! Row endpoints exchange plain JSON objects, but the query layer speaks
//! [`Value`]. These two helpers bridge them: [`json_to_value`] coerces an
//! incoming JSON scalar to the [`Value`] variant a column's [`BasicType`] calls
//! for (so a `timestamptz` column receives a real timestamp, not a string), and
//! [`value_to_json`] renders a value read back from the database as natural JSON
//! (not the tagged `{"type":…,"value":…}` form `Value`'s own `Serialize` emits).
//!
//! ## Why it is *here*
//!
//! It started in `sc-api`, whose row endpoints are its heaviest user, and moved
//! down when a second, much lower caller appeared: a trigger's `only_if` reads an
//! event's row, which arrives as JSON and must be **typed by its columns** before
//! a Ⱶ-path prefetch can correlate on it (a `uuid` key compared as text is a SQL
//! error, not a mismatch). That is a fact about types and values, not about an
//! API, so it belongs at layer 3 where both callers can reach it.
//! `sc_api::convert` re-exports both names, so the API side is unchanged.
//!
//! Not to be confused with [`sc_expr::value_to_json`], which renders the *same*
//! values for **JavaScript**: a decimal becomes a number there and a string here,
//! because one feeds arithmetic in an isolate and the other a wire response that
//! must not lose precision.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use rust_decimal::Decimal;
use sc_error::{Error, Result};
use sc_query::Value;
use serde_json::Value as Json;

use crate::BasicType;
use std::str::FromStr;
use uuid::Uuid;

/// Render a [`Value`] read from the database as natural JSON for an API response.
///
/// Temporal and decimal values become strings (ISO-8601 / plain decimal) so no
/// precision is lost; `Bytes` becomes an array of byte values; `Json` passes
/// through unchanged.
pub fn value_to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
        Value::Text(s) => Json::String(s.clone()),
        Value::Bytes(b) => Json::Array(b.iter().map(|byte| Json::from(*byte)).collect()),
        Value::Json(j) => j.clone(),
        Value::Uuid(u) => Json::String(u.to_string()),
        Value::Date(d) => Json::String(d.to_string()),
        Value::Time(t) => Json::String(t.to_string()),
        Value::Timestamp(ts) => Json::String(ts.to_rfc3339()),
        Value::Decimal(d) => Json::String(d.to_string()),
    }
}

/// Coerce an incoming JSON scalar to the [`Value`] variant a column of the given
/// [`BasicType`] expects.
///
/// JSON `null` always maps to [`Value::Null`]. Otherwise the target type drives
/// the parse: a string destined for a `uuid`/`date`/`timestamptz`/`numeric`
/// column is parsed into the corresponding value (an [`Error::invalid`] if it is
/// malformed), while a JSON object/array for a `json` column is embedded
/// verbatim. Types without a stricter target fall back to the JSON scalar's
/// natural mapping.
pub fn json_to_value(basic: &BasicType, json: &Json) -> Result<Value> {
    if json.is_null() {
        return Ok(Value::Null);
    }
    match basic {
        BasicType::Bool => match json {
            Json::Bool(b) => Ok(Value::Bool(*b)),
            _ => Err(type_error("bool", json)),
        },
        BasicType::Int => match json {
            Json::Number(n) => n
                .as_i64()
                .map(Value::Int)
                .ok_or_else(|| type_error("int", json)),
            Json::String(s) => s
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| type_error("int", json)),
            _ => Err(type_error("int", json)),
        },
        BasicType::Float => match json {
            Json::Number(n) => n
                .as_f64()
                .map(Value::Float)
                .ok_or_else(|| type_error("float", json)),
            Json::String(s) => s
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| type_error("float", json)),
            _ => Err(type_error("float", json)),
        },
        BasicType::Decimal => parse_str(json, "decimal", |s| {
            Decimal::from_str(s).map(Value::Decimal).ok()
        }),
        BasicType::Uuid => parse_str(json, "uuid", |s| Uuid::parse_str(s).map(Value::Uuid).ok()),
        BasicType::Date => parse_str(json, "date", |s| {
            NaiveDate::from_str(s).map(Value::Date).ok()
        }),
        BasicType::Time => parse_str(json, "time", |s| {
            NaiveTime::from_str(s).map(Value::Time).ok()
        }),
        BasicType::Timestamp => parse_str(json, "timestamp", |s| {
            DateTime::parse_from_rfc3339(s)
                .map(|dt| Value::Timestamp(dt.with_timezone(&Utc)))
                .ok()
        }),
        BasicType::Json => Ok(Value::Json(json.clone())),
        // A geometry is a GeoJSON object; a form or a CSV cell sends it as the
        // object's text.
        BasicType::Geometry(kind) => match json {
            Json::String(s) => {
                let parsed: Json = serde_json::from_str(s).map_err(|_| {
                    Error::invalid(format!(
                        "{s:?} is not {}: a geometry is written as GeoJSON",
                        kind.describe()
                    ))
                })?;
                crate::geometry::geometry_value(*kind, &parsed)
            }
            other => crate::geometry::geometry_value(*kind, other),
        },
        BasicType::Bytes => match json {
            Json::Array(items) => {
                let mut bytes = Vec::with_capacity(items.len());
                for item in items {
                    let byte = item
                        .as_u64()
                        .and_then(|n| u8::try_from(n).ok())
                        .ok_or_else(|| type_error("bytes", json))?;
                    bytes.push(byte);
                }
                Ok(Value::Bytes(bytes))
            }
            _ => Err(type_error("bytes", json)),
        },
        BasicType::Text | BasicType::Other(_) => match json {
            Json::String(s) => Ok(Value::Text(s.clone())),
            // A non-string for a text column is stringified rather than rejected,
            // matching the catch-all fieldview's lenient display path.
            other => Ok(Value::Text(other.to_string())),
        },
    }
}

/// Apply a string parser to a JSON string, erroring for a non-string or a parse
/// failure.
fn parse_str(json: &Json, ty: &str, parse: impl Fn(&str) -> Option<Value>) -> Result<Value> {
    match json {
        Json::String(s) => parse(s).ok_or_else(|| type_error(ty, json)),
        _ => Err(type_error(ty, json)),
    }
}

/// An [`Error::invalid`] describing a value that does not fit its column type.
fn type_error(ty: &str, json: &Json) -> Error {
    Error::invalid(format!("value {json} is not a valid {ty}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_scalar_types() {
        let uuid = Uuid::new_v4();
        let cases = [
            (BasicType::Bool, Json::Bool(true), Value::Bool(true)),
            (BasicType::Int, Json::from(7_i64), Value::Int(7)),
            (
                BasicType::Text,
                Json::String("hi".into()),
                Value::Text("hi".into()),
            ),
            (
                BasicType::Uuid,
                Json::String(uuid.to_string()),
                Value::Uuid(uuid),
            ),
        ];
        for (ty, json, expected) in cases {
            let value = json_to_value(&ty, &json).unwrap();
            assert_eq!(value, expected);
            assert_eq!(value_to_json(&value), json);
        }
    }

    #[test]
    fn null_maps_regardless_of_type() {
        assert_eq!(
            json_to_value(&BasicType::Int, &Json::Null).unwrap(),
            Value::Null
        );
        assert_eq!(value_to_json(&Value::Null), Json::Null);
    }

    #[test]
    fn rejects_malformed_typed_string() {
        assert!(json_to_value(&BasicType::Uuid, &Json::String("nope".into())).is_err());
        assert!(json_to_value(&BasicType::Int, &Json::String("x".into())).is_err());
    }

    #[test]
    fn text_column_stringifies_non_strings() {
        let v = json_to_value(&BasicType::Text, &Json::from(3_i64)).unwrap();
        assert_eq!(v, Value::Text("3".into()));
    }
}
