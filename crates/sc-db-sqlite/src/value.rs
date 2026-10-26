//! Mapping between the query layer's [`Value`] and SQLite's five storage
//! classes.
//!
//! SQLite is dynamically typed: a value is NULL, an integer, a real, text or a
//! blob, and a *column* has only an affinity nudging values towards one of them.
//! The eleven things a Saltcorn [`Value`] can be therefore do not survive a
//! round trip on storage class alone — a timestamp and a uuid are both text, a
//! bool and a row count are both integers.
//!
//! What closes the gap is the **declared type**. SQLite keeps the type name a
//! column was declared with verbatim and reports it back (`PRAGMA table_info`,
//! `sqlite3_column_decltype`), so a column this driver created as `timestamptz`
//! still says `timestamptz` when it is read — and [`decode`] uses that name to
//! turn the text back into a [`Value::Timestamp`]. This is why [`crate::ddl`]
//! emits the *given* type name rather than translating it to one of SQLite's
//! five: the name is the only place the distinction can live, and translating it
//! would throw away the information needed to read the value back.
//!
//! A column with no declared type — every expression in a projection, an
//! aggregate, a `CASE` — decodes by storage class alone, which is the honest
//! answer: SQLite has not committed to a type there either.
//!
//! **Text encodings are fixed and sortable.** A timestamp is written
//! `YYYY-MM-DDTHH:MM:SS.mmmZ`, always UTC and always the same width, so
//! lexicographic order — which is what `ORDER BY` on a text column gives — is
//! chronological order. A date is `YYYY-MM-DD` and a time `HH:MM:SS.mmm` for the
//! same reason. Reading is deliberately more lenient than writing: a file
//! somebody else created may hold `2024-05-06 12:00:00`, and refusing to read it
//! would make "point Saltcorn at an existing SQLite file" a promise this driver
//! did not keep.

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rust_decimal::Decimal;
use sc_error::{Error, Result};
use sc_query::Value;
use uuid::Uuid;

/// How a timestamp is written: UTC, milliseconds, fixed width — so text order is
/// time order.
pub(crate) const TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3fZ";
/// How a date is written.
pub(crate) const DATE_FORMAT: &str = "%Y-%m-%d";
/// How a time of day is written.
pub(crate) const TIME_FORMAT: &str = "%H:%M:%S%.3f";

/// What a column's *declared type* says a value in it means.
///
/// Derived from the type name rather than from SQLite's affinity rules, because
/// affinity has five outcomes and this has to distinguish eleven. The names
/// recognised are both the ones this driver writes (which are `sc-types`'
/// canonical SQL names, so `jsonb` and `timestamptz` as much as `text`) and the
/// ones a SQLite file created by anything else carries (`INTEGER`, `VARCHAR(64)`,
/// `DATETIME`, `BLOB`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Declared {
    Bool,
    Int,
    Float,
    Decimal,
    Text,
    Bytes,
    Json,
    Uuid,
    Date,
    Time,
    Timestamp,
    /// A vector (`vector(n)`, TODO-agent-skills AS2.1): a blob of
    /// little-endian `f32`s, read back as a JSON array of numbers.
    Vector,
    /// No declared type, or one this driver has no reading for — decode by
    /// storage class.
    Unknown,
}

/// The [`Declared`] meaning of a declared type name.
///
/// A parameterised name (`varchar(64)`, `numeric(10,2)`) is matched on the part
/// before the parenthesis, which is what SQLite's own affinity rules do.
pub(crate) fn declared(decl_type: &str) -> Declared {
    let name = decl_type
        .split('(')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match name.as_str() {
        "" => Declared::Unknown,
        "bool" | "boolean" => Declared::Bool,
        "int" | "int2" | "int4" | "int8" | "integer" | "smallint" | "bigint" | "tinyint"
        | "mediumint" | "serial" | "bigserial" | "smallserial" => Declared::Int,
        "float" | "float4" | "float8" | "real" | "double" | "double precision" => Declared::Float,
        "numeric" | "decimal" | "money" => Declared::Decimal,
        "text" | "varchar" | "character varying" | "char" | "character" | "bpchar" | "clob"
        | "name" | "citext" | "nvarchar" | "nchar" => Declared::Text,
        "bytea" | "blob" => Declared::Bytes,
        "json" | "jsonb" => Declared::Json,
        "uuid" => Declared::Uuid,
        "date" => Declared::Date,
        "time" | "timetz" | "time without time zone" | "time with time zone" => Declared::Time,
        "timestamp"
        | "timestamptz"
        | "datetime"
        | "timestamp without time zone"
        | "timestamp with time zone" => Declared::Timestamp,
        "vector" => Declared::Vector,
        _ => Declared::Unknown,
    }
}

/// A [`Value`] as the SQLite value it is stored as.
///
/// Owned rather than borrowed because the binds outlive the render: rusqlite
/// takes `ToSql` implementors, and `rusqlite::types::Value` is the one that
/// needs no lifetime. [`Value::Null`] binds as SQL `NULL` for any column, as it
/// does everywhere.
pub(crate) fn bind(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        // No boolean storage class: SQLite's own `true`/`false` keywords are
        // 1 and 0, and that is what a `bool` column holds.
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Int(i) => SqlValue::Integer(*i),
        Value::Float(f) => SqlValue::Real(*f),
        Value::Text(t) => SqlValue::Text(t.clone()),
        Value::Bytes(b) => SqlValue::Blob(b.clone()),
        Value::Json(j) => SqlValue::Text(j.to_string()),
        Value::Uuid(u) => SqlValue::Text(u.to_string()),
        Value::Date(d) => SqlValue::Text(d.format(DATE_FORMAT).to_string()),
        Value::Time(t) => SqlValue::Text(t.format(TIME_FORMAT).to_string()),
        Value::Timestamp(ts) => SqlValue::Text(ts.format(TIMESTAMP_FORMAT).to_string()),
        // As text, not as a real: a decimal that went out through an f64 would
        // come back a different number, which is the one thing the type exists
        // to prevent. A `numeric`-affinity column converts it to a real on the
        // way in only when it fits one exactly.
        Value::Decimal(d) => SqlValue::Text(d.to_string()),
    }
}

/// One column of a result row as a [`Value`], read through what its column was
/// declared as.
///
/// `decl` is the declared type of the result column when SQLite reports one (a
/// plain table column), and `None` for an expression. A value whose storage
/// class contradicts the declaration is decoded by storage class rather than
/// refused: SQLite lets any value into any column, so a file written by another
/// program may hold text in an `int8` column, and an error there would make the
/// row unreadable rather than the cell.
pub(crate) fn decode(raw: ValueRef<'_>, decl: Declared) -> Result<Value> {
    Ok(match raw {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => match decl {
            Declared::Bool => Value::Bool(i != 0),
            Declared::Float => Value::Float(i as f64),
            Declared::Decimal => Value::Decimal(Decimal::from(i)),
            Declared::Json => Value::Json(serde_json::Value::from(i)),
            // A timestamp column holding an integer is a unix time — what a
            // program that stored `strftime('%s')` left behind.
            Declared::Timestamp => Utc
                .timestamp_opt(i, 0)
                .single()
                .map_or(Value::Int(i), Value::Timestamp),
            _ => Value::Int(i),
        },
        ValueRef::Real(f) => match decl {
            Declared::Decimal => Decimal::try_from(f).map_or(Value::Float(f), Value::Decimal),
            Declared::Int => Value::Int(f as i64),
            Declared::Json => serde_json::Number::from_f64(f).map_or(Value::Float(f), |n| {
                Value::Json(serde_json::Value::Number(n))
            }),
            _ => Value::Float(f),
        },
        ValueRef::Text(bytes) => {
            let text = std::str::from_utf8(bytes)
                .map_err(|e| Error::database(format!("column is not valid UTF-8 text: {e}")))?;
            decode_text(text, decl)
        }
        ValueRef::Blob(bytes) => match decl {
            Declared::Vector => {
                vector_from_blob(bytes).unwrap_or_else(|| Value::Bytes(bytes.to_vec()))
            }
            _ => Value::Bytes(bytes.to_vec()),
        },
    })
}

/// A vector blob as the JSON array a vector is above the driver, or `None`
/// when its length is not a whole number of `f32`s (and the cell is then its
/// bytes, as any surprising cell is — see [`decode`]).
fn vector_from_blob(bytes: &[u8]) -> Option<Value> {
    if bytes.len() % 4 != 0 {
        return None;
    }
    let values: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Some(Value::Json(serde_json::Value::Array(
        values
            .iter()
            // The shortest decimal that reads back as the same `f32`, so 0.1
            // written is 0.1 read.
            .map(|x| {
                format!("{x}")
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map_or(serde_json::Value::Null, serde_json::Value::Number)
            })
            .collect(),
    )))
}

/// Text read through its declared type, falling back to the text itself
/// whenever it does not parse — see [`decode`] on why a surprising cell is not
/// an unreadable row.
fn decode_text(text: &str, decl: Declared) -> Value {
    match decl {
        Declared::Bool => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "1" => Value::Bool(true),
            "false" | "f" | "no" | "n" | "0" => Value::Bool(false),
            _ => Value::Text(text.to_owned()),
        },
        Declared::Int => text
            .trim()
            .parse::<i64>()
            .map_or_else(|_| Value::Text(text.to_owned()), Value::Int),
        Declared::Float => text
            .trim()
            .parse::<f64>()
            .map_or_else(|_| Value::Text(text.to_owned()), Value::Float),
        Declared::Decimal => text
            .trim()
            .parse::<Decimal>()
            .map_or_else(|_| Value::Text(text.to_owned()), Value::Decimal),
        Declared::Json | Declared::Vector => {
            serde_json::from_str(text).map_or_else(|_| Value::Text(text.to_owned()), Value::Json)
        }
        Declared::Uuid => {
            Uuid::parse_str(text.trim()).map_or_else(|_| Value::Text(text.to_owned()), Value::Uuid)
        }
        Declared::Date => {
            parse_date(text).map_or_else(|| Value::Text(text.to_owned()), Value::Date)
        }
        Declared::Time => {
            parse_time(text).map_or_else(|| Value::Text(text.to_owned()), Value::Time)
        }
        Declared::Timestamp => {
            parse_timestamp(text).map_or_else(|| Value::Text(text.to_owned()), Value::Timestamp)
        }
        Declared::Bytes => Value::Bytes(text.as_bytes().to_vec()),
        Declared::Text | Declared::Unknown => Value::Text(text.to_owned()),
    }
}

/// A date in the form this driver writes, or the other common spelling.
fn parse_date(text: &str) -> Option<NaiveDate> {
    let text = text.trim();
    NaiveDate::parse_from_str(text, DATE_FORMAT)
        .ok()
        // A datetime in a date column: take its date, which is what was meant.
        .or_else(|| parse_timestamp(text).map(|ts| ts.date_naive()))
}

/// A time of day, with or without fractional seconds.
fn parse_time(text: &str) -> Option<NaiveTime> {
    let text = text.trim();
    NaiveTime::parse_from_str(text, "%H:%M:%S%.f")
        .or_else(|_| NaiveTime::parse_from_str(text, "%H:%M"))
        .ok()
}

/// A timestamp in this driver's own format, in RFC 3339, or in SQLite's
/// space-separated `datetime()` output — always read as UTC when it carries no
/// zone, which is what `CURRENT_TIMESTAMP` produces.
fn parse_timestamp(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    if let Ok(ts) = DateTime::parse_from_rfc3339(text) {
        return Some(ts.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, format) {
            return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_types_cover_what_this_driver_writes_and_what_it_may_find() {
        // What `sc-types` calls a column, which is what `ddl` emits verbatim.
        assert_eq!(declared("timestamptz"), Declared::Timestamp);
        assert_eq!(declared("jsonb"), Declared::Json);
        assert_eq!(declared("int8"), Declared::Int);
        assert_eq!(declared("bytea"), Declared::Bytes);
        // What a file created by something else carries.
        assert_eq!(declared("INTEGER"), Declared::Int);
        assert_eq!(declared("VARCHAR(64)"), Declared::Text);
        assert_eq!(declared("DATETIME"), Declared::Timestamp);
        assert_eq!(declared("BLOB"), Declared::Bytes);
        assert_eq!(declared("NUMERIC(10,2)"), Declared::Decimal);
        // An expression has no declared type at all.
        assert_eq!(declared(""), Declared::Unknown);
        assert_eq!(declared("geography"), Declared::Unknown);
    }

    #[test]
    fn a_timestamp_is_written_fixed_width_utc_so_text_order_is_time_order() {
        let early = Utc.with_ymd_and_hms(2024, 5, 6, 9, 0, 0).unwrap();
        let late = Utc.with_ymd_and_hms(2024, 5, 6, 12, 0, 0).unwrap();
        let (SqlValue::Text(a), SqlValue::Text(b)) = (
            bind(&Value::Timestamp(early)),
            bind(&Value::Timestamp(late)),
        ) else {
            panic!("a timestamp binds as text");
        };
        assert_eq!(a, "2024-05-06T09:00:00.000Z");
        assert!(a < b, "{a} should sort before {b}");
    }

    #[test]
    fn values_round_trip_through_their_declared_type() {
        let cases = vec![
            (Value::Bool(true), Declared::Bool),
            (Value::Int(42), Declared::Int),
            (Value::Float(2.5), Declared::Float),
            (Value::Text("hi".into()), Declared::Text),
            (Value::Bytes(vec![1, 2, 3]), Declared::Bytes),
            (Value::Json(serde_json::json!({"a": 1})), Declared::Json),
            (Value::Uuid(Uuid::nil()), Declared::Uuid),
            (
                Value::Date(NaiveDate::from_ymd_opt(2024, 5, 6).unwrap()),
                Declared::Date,
            ),
            (
                Value::Time(NaiveTime::from_hms_opt(12, 30, 0).unwrap()),
                Declared::Time,
            ),
            (
                Value::Timestamp(Utc.with_ymd_and_hms(2024, 5, 6, 12, 0, 0).unwrap()),
                Declared::Timestamp,
            ),
            (Value::Decimal(Decimal::new(12345, 2)), Declared::Decimal),
        ];
        for (value, decl) in cases {
            let stored = bind(&value);
            let back = decode(ValueRef::from(&stored), decl).expect("decode");
            assert_eq!(back, value, "round trip of {value:?} as {decl:?}");
        }
    }

    #[test]
    fn an_undeclared_column_decodes_by_storage_class() {
        // What an aggregate or a CASE arm comes back as.
        let n = decode(ValueRef::Integer(3), Declared::Unknown).unwrap();
        assert_eq!(n, Value::Int(3));
        let t = decode(ValueRef::Text(b"x"), Declared::Unknown).unwrap();
        assert_eq!(t, Value::Text("x".into()));
        let f = decode(ValueRef::Real(1.5), Declared::Unknown).unwrap();
        assert_eq!(f, Value::Float(1.5));
        assert_eq!(
            decode(ValueRef::Null, Declared::Timestamp).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn a_foreign_files_spellings_are_read_rather_than_refused() {
        // `CURRENT_TIMESTAMP`'s own output, and a unix time.
        let ts = decode(ValueRef::Text(b"2024-05-06 12:00:00"), Declared::Timestamp).unwrap();
        assert_eq!(
            ts,
            Value::Timestamp(Utc.with_ymd_and_hms(2024, 5, 6, 12, 0, 0).unwrap())
        );
        let unix = decode(ValueRef::Integer(1_714_996_800), Declared::Timestamp).unwrap();
        assert!(matches!(unix, Value::Timestamp(_)));
        // A bool column written by a program that stored the word.
        assert_eq!(
            decode(ValueRef::Text(b"true"), Declared::Bool).unwrap(),
            Value::Bool(true)
        );
        // And a cell that contradicts its column is the cell's own value, not an
        // unreadable row.
        assert_eq!(
            decode(ValueRef::Text(b"not a date"), Declared::Date).unwrap(),
            Value::Text("not a date".into())
        );
    }
}
