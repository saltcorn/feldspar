//! Converting between JSON and the query layer's [`Value`]: the one module
//! that does it (technical design §13.2, §4).
//!
//! ## Four readers, four spellings
//!
//! A value is written as JSON for four readers, and they want four slightly
//! different things. The differences are deliberate, so they are kept here, side
//! by side, as the cases of [`Spelling`], rather than as four functions in four
//! crates:
//!
//! | value | [`Wire`](Spelling::Wire) | [`Script`](Spelling::Script) | [`Analysis`](Spelling::Analysis) | [`Archive`](Spelling::Archive) |
//! |---|---|---|---|---|
//! | decimal | string (exact) | number (`f64`) | number (`f64`, parsed from its digits) | string (exact) |
//! | bytes | array of numbers | `null` | array of numbers | base64 string |
//! | NaN, ±∞ | `null` | `null` | `null` | `"NaN"`, `"Infinity"`, `"-Infinity"` |
//! | timestamp | RFC 3339, as many sub-second digits as chrono picks | the same | the same | RFC 3339 in UTC (`Z`), every sub-second digit |
//!
//! Everything else — null, a boolean, an integer as an exact JSON number, text,
//! a UUID, a date, a time, JSON as itself — is spelt the same for all four.
//!
//! - **Wire** ([`value_to_json`]): a typed API response and the admin API — a
//!   decimal must not lose its last digit on the way to a screen.
//! - **Script**: what a formula or a code body sees in JavaScript, where a
//!   decimal feeds arithmetic. An `i64` beyond 2^53 loses precision there (JS
//!   numbers are `f64`), and a `bytea` column has no JS literal.
//! - **Analysis**: a dataset's grid and plots: numbers as numbers, including a
//!   decimal, and otherwise the wire's spelling.
//! - **Archive**: a backup. The wire's bytes cost about 32 bytes of parsed JSON
//!   per byte of data and its `null` for a non-finite float loses the value, so
//!   a backup writes base64 and names the non-finite floats; a timestamp keeps
//!   its nanoseconds.
//!
//! ## Reading
//!
//! [`json_to_value`] coerces JSON to the [`Value`] a column's [`BasicType`]
//! calls for (so a `timestamptz` column receives a real timestamp, not a
//! string); [`json_to_value_as`] reads a [`Spelling`], which differs from it only
//! for an archive's bytes. [`value_from_json`] reads JSON that has **no column
//! behind it** — a trigger event's row, the caller object — as the value its own
//! shape implies.
//!
//! These live at layer 3 because a trigger's `only_if` types an event's row by
//! its columns, three layers below any API, and the formula engine (`sc-expr`)
//! renders values for JavaScript.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, NaiveDate, NaiveTime, SecondsFormat, Utc};
use rust_decimal::Decimal;
use sc_error::{Error, Result};
use sc_query::Value;
use serde_json::Value as Json;

use crate::BasicType;
use std::str::FromStr;
use uuid::Uuid;

/// Who a value is spelt for. See the [module documentation](self) for the
/// table of what differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spelling {
    /// An API response: nothing is lost, a decimal is a string.
    Wire,
    /// JavaScript (a formula, a code body): a decimal is a number, bytes are
    /// `null`.
    Script,
    /// A dataset's grid: a decimal is a number, the rest as on the wire.
    Analysis,
    /// A backup: bytes in base64, non-finite floats named, timestamps in full.
    Archive,
}

/// Render a [`Value`] read from the database as natural JSON for an API response
/// — the [`Wire`](Spelling::Wire) spelling.
///
/// Temporal and decimal values become strings (ISO-8601 / plain decimal) so no
/// precision is lost; `Bytes` becomes an array of byte values; `Json` passes
/// through unchanged. Not the tagged `{"type":…,"value":…}` form `Value`'s own
/// `Serialize` emits.
pub fn value_to_json(value: &Value) -> Json {
    value_to_json_as(value, Spelling::Wire)
}

/// [`value_to_json_as`] in the [`Script`](Spelling::Script) spelling: a value as
/// JavaScript sees it. Named for passing to `map`.
pub fn value_to_script_json(value: &Value) -> Json {
    value_to_json_as(value, Spelling::Script)
}

/// [`value_to_json_as`] in the [`Analysis`](Spelling::Analysis) spelling: a value
/// as a dataset's grid shows it. Named for passing to `map`.
pub fn value_to_analysis_json(value: &Value) -> Json {
    value_to_json_as(value, Spelling::Analysis)
}

/// Render a [`Value`] as JSON for `spelling`'s reader.
pub fn value_to_json_as(value: &Value, spelling: Spelling) -> Json {
    match (spelling, value) {
        (_, Value::Null) => Json::Null,
        (_, Value::Bool(b)) => Json::Bool(*b),
        (_, Value::Int(i)) => Json::from(*i),
        (Spelling::Archive, Value::Float(f)) if f.is_nan() => Json::String("NaN".to_owned()),
        (Spelling::Archive, Value::Float(f)) if f.is_infinite() => Json::String(
            if f.is_sign_positive() {
                "Infinity"
            } else {
                "-Infinity"
            }
            .to_owned(),
        ),
        (_, Value::Float(f)) => number(*f),
        (_, Value::Text(s)) => Json::String(s.clone()),
        (Spelling::Script, Value::Bytes(_)) => Json::Null,
        (Spelling::Archive, Value::Bytes(b)) => Json::String(BASE64.encode(b)),
        (Spelling::Wire | Spelling::Analysis, Value::Bytes(b)) => {
            Json::Array(b.iter().map(|byte| Json::from(*byte)).collect())
        }
        (_, Value::Json(j)) => j.clone(),
        (_, Value::Uuid(u)) => Json::String(u.to_string()),
        (_, Value::Date(d)) => Json::String(d.to_string()),
        (_, Value::Time(t)) => Json::String(t.to_string()),
        (Spelling::Archive, Value::Timestamp(ts)) => {
            Json::String(ts.to_rfc3339_opts(SecondsFormat::AutoSi, true))
        }
        (_, Value::Timestamp(ts)) => Json::String(ts.to_rfc3339()),
        (Spelling::Wire | Spelling::Archive, Value::Decimal(d)) => Json::String(d.to_string()),
        // Two ways to a float, kept as they were: rust_decimal's own conversion
        // for a formula, and the correctly rounded parse of the digits for a grid.
        // They can differ in the last bit, and a formula's result must not change.
        (Spelling::Script, Value::Decimal(d)) => number(f64::try_from(*d).unwrap_or(f64::NAN)),
        (Spelling::Analysis, Value::Decimal(d)) => {
            number(d.to_string().parse::<f64>().unwrap_or(f64::NAN))
        }
    }
}

/// A float as a JSON number, or `null` where JSON has none (NaN, ±∞).
fn number(f: f64) -> Json {
    serde_json::Number::from_f64(f).map_or(Json::Null, Json::Number)
}

/// A JSON value read as the [`Value`] its own shape implies — the inverse of
/// the [`Script`](Spelling::Script) spelling as far as JSON can express it (a
/// timestamp that became a string comes back as text).
///
/// This is for values that arrive as JSON with **no column behind them**: a
/// trigger event's row, the caller object, an action's computed setting. Where a
/// column *is* known the caller should coerce to its type instead
/// ([`json_to_value`]), which recovers the uuid, timestamp and decimal this
/// leaves as text — the distinction matters only for SQL translation, since a
/// reified evaluation renders both back to the same JS value.
pub fn value_from_json(json: &Json) -> Value {
    match json {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Value::Int(i),
            (None, Some(f)) => Value::Float(f),
            // Not representable as either: `serde_json` cannot produce this, and
            // carrying the digits as text loses nothing that was there.
            (None, None) => Value::Text(n.to_string()),
        },
        Json::String(s) => Value::Text(s.clone()),
        // A composite has one faithful `Value`, and it is the JSON itself.
        Json::Array(_) | Json::Object(_) => Value::Json(json.clone()),
    }
}

/// Read JSON written in `spelling` as the value a column of type `basic` holds.
///
/// Only an [archive](Spelling::Archive)'s bytes read differently from
/// [`json_to_value`]: a base64 string. Its named non-finite floats are already
/// read by the float parser.
pub fn json_to_value_as(basic: &BasicType, json: &Json, spelling: Spelling) -> Result<Value> {
    match (spelling, basic, json) {
        (Spelling::Archive, BasicType::Bytes, Json::String(text)) => BASE64
            .decode(text)
            .map(Value::Bytes)
            .map_err(|e| Error::invalid(format!("the bytes are not valid base64: {e}"))),
        (Spelling::Archive, BasicType::Bytes, Json::Null) => Ok(Value::Null),
        (Spelling::Archive, BasicType::Bytes, other) => Err(Error::invalid(format!(
            "bytes are written as a base64 string, not {}",
            kind_of(other)
        ))),
        _ => json_to_value(basic, json),
    }
}

/// What kind of JSON value `json` is, as a sentence says it.
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
            // The text a query string carries (`?done=eq.false`), as `"7"` is
            // for an int column. Only the two JSON spellings: the looser
            // `t`/`1`/`yes` a form submits is the catch-all fieldview's.
            Json::String(s) if s == "true" => Ok(Value::Bool(true)),
            Json::String(s) if s == "false" => Ok(Value::Bool(false)),
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
        // A full instant is accepted too, as its UTC calendar date — what a
        // JavaScript `Date` (a `new Date()` default) serialises to.
        BasicType::Date => parse_str(json, "date", |s| {
            NaiveDate::from_str(s)
                .ok()
                .or_else(|| {
                    DateTime::parse_from_rfc3339(s)
                        .ok()
                        .map(|dt| dt.with_timezone(&Utc).date_naive())
                })
                .map(Value::Date)
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
        // A vector is an array of numbers of the field's length; a form or a
        // CSV cell sends the array's text.
        BasicType::Vector(dimensions) => crate::vector::vector_value(*dimensions, json),
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
    fn a_bool_column_takes_true_and_false_as_text_and_nothing_looser() {
        let bool_of = |s: &str| json_to_value(&BasicType::Bool, &Json::String(s.into()));
        assert_eq!(bool_of("true").unwrap(), Value::Bool(true));
        assert_eq!(bool_of("false").unwrap(), Value::Bool(false));
        for refused in ["t", "f", "0", "1", "yes", "TRUE", " false", ""] {
            assert!(bool_of(refused).is_err(), "`{refused}` was accepted");
        }
        assert!(json_to_value(&BasicType::Bool, &Json::from(0_i64)).is_err());
    }

    #[test]
    fn rejects_malformed_typed_string() {
        assert!(json_to_value(&BasicType::Uuid, &Json::String("nope".into())).is_err());
        assert!(json_to_value(&BasicType::Int, &Json::String("x".into())).is_err());
    }

    #[test]
    fn a_date_column_takes_an_instant_as_its_utc_date() {
        let date = |s: &str| json_to_value(&BasicType::Date, &Json::String(s.into()));
        let expected = Value::Date(NaiveDate::from_ymd_opt(2026, 3, 4).unwrap());
        assert_eq!(date("2026-03-04").unwrap(), expected);
        assert_eq!(date("2026-03-04T10:00:00.000Z").unwrap(), expected);
        assert_eq!(
            date("2026-03-03T23:30:00-01:00").unwrap(),
            Value::Date(NaiveDate::from_ymd_opt(2026, 3, 4).unwrap())
        );
        assert!(date("2026-03-04 nonsense").is_err());
    }

    #[test]
    fn text_column_stringifies_non_strings() {
        let v = json_to_value(&BasicType::Text, &Json::from(3_i64)).unwrap();
        assert_eq!(v, Value::Text("3".into()));
    }

    #[test]
    fn a_typeless_json_value_reads_as_its_own_shape_and_round_trips() {
        use serde_json::json;
        assert_eq!(value_from_json(&json!(null)), Value::Null);
        assert_eq!(value_from_json(&json!(false)), Value::Bool(false));
        assert_eq!(value_from_json(&json!(4)), Value::Int(4));
        assert_eq!(value_from_json(&json!(1.5)), Value::Float(1.5));
        assert_eq!(value_from_json(&json!("x")), Value::Text("x".into()));
        let obj = json!({ "a": [1, 2] });
        assert_eq!(value_from_json(&obj), Value::Json(obj.clone()));
        // The pair agrees in both directions for everything JSON can say, which
        // is what lets an event's row be bound without a column to type it.
        for value in [
            json!(null),
            json!(true),
            json!(2),
            json!(1.5),
            json!("s"),
            obj,
        ] {
            assert_eq!(
                value_to_json_as(&value_from_json(&value), Spelling::Script),
                value
            );
        }
    }

    /// The module documentation's table, cell by cell: the four spellings agree
    /// on everything but decimals, bytes, non-finite floats and timestamps.
    #[test]
    fn the_four_spellings_differ_only_where_the_table_says() {
        use chrono::TimeZone;
        use serde_json::json;
        let all = [
            Spelling::Wire,
            Spelling::Script,
            Spelling::Analysis,
            Spelling::Archive,
        ];
        let spell = |v: &Value| all.map(|s| value_to_json_as(v, s));

        let decimal = Value::Decimal(Decimal::from_str("12.50").unwrap());
        assert_eq!(
            spell(&decimal),
            [json!("12.50"), json!(12.5), json!(12.5), json!("12.50")]
        );
        let bytes = Value::Bytes(b"hi".to_vec());
        assert_eq!(
            spell(&bytes),
            [
                json!([104, 105]),
                json!(null),
                json!([104, 105]),
                json!("aGk=")
            ]
        );
        assert_eq!(
            spell(&Value::Float(f64::NAN)),
            [json!(null), json!(null), json!(null), json!("NaN")]
        );
        assert_eq!(spell(&Value::Float(f64::INFINITY))[3], json!("Infinity"));
        let ts = Value::Timestamp(
            Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap() + chrono::Duration::nanoseconds(7),
        );
        let [wire, script, analysis, archive] = spell(&ts);
        assert_eq!(wire, json!("2026-01-02T03:04:05.000000007+00:00"));
        assert_eq!(script, wire);
        assert_eq!(analysis, wire);
        assert_eq!(archive, json!("2026-01-02T03:04:05.000000007Z"));

        let uuid = Uuid::new_v4();
        for same in [
            Value::Null,
            Value::Bool(true),
            Value::Int(i64::MAX),
            Value::Float(0.25),
            Value::Text("t".into()),
            Value::Uuid(uuid),
            Value::Date(NaiveDate::from_ymd_opt(2026, 3, 4).unwrap()),
            Value::Time(NaiveTime::from_hms_opt(1, 2, 3).unwrap()),
            Value::Json(json!({"a": [1]})),
        ] {
            let [wire, rest @ ..] = spell(&same);
            assert!(rest.iter().all(|r| *r == wire), "{same:?}");
        }
    }

    #[test]
    fn an_archive_reads_bytes_as_base64_and_everything_else_as_the_wire_does() {
        assert_eq!(
            json_to_value_as(&BasicType::Bytes, &Json::from("aGk="), Spelling::Archive).unwrap(),
            Value::Bytes(b"hi".to_vec())
        );
        assert!(
            json_to_value_as(
                &BasicType::Bytes,
                &Json::from("not base64!"),
                Spelling::Archive
            )
            .is_err()
        );
        assert!(json_to_value_as(&BasicType::Bytes, &Json::from(7), Spelling::Archive).is_err());
        assert_eq!(
            json_to_value_as(&BasicType::Bytes, &Json::Null, Spelling::Archive).unwrap(),
            Value::Null
        );
        match json_to_value_as(
            &BasicType::Float,
            &Json::from("-Infinity"),
            Spelling::Archive,
        ) {
            Ok(Value::Float(f)) => assert!(f.is_infinite() && f.is_sign_negative()),
            other => panic!("{other:?}"),
        }
        // On the wire, bytes are an array of numbers.
        assert_eq!(
            json_to_value_as(
                &BasicType::Bytes,
                &serde_json::json!([1, 2]),
                Spelling::Wire
            )
            .unwrap(),
            Value::Bytes(vec![1, 2])
        );
    }
}
