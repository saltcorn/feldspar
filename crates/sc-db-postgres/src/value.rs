//! Wire-level mapping between the query layer's [`Value`] and tokio-postgres.
//!
//! Two directions, both **basic types only** for this milestone (rich types and
//! their fieldviews are a later phase):
//!
//! - **Bind → parameter**: [`PgParam`] wraps a `&Value` and implements
//!   [`ToSql`], so the ordered binds produced by rendering a statement can be
//!   passed straight to `Client::query`. Every literal already left the SQL as a
//!   placeholder (the query layer's injection guarantee); this just encodes the
//!   value the placeholder points at. A [`Value::Null`] encodes as SQL `NULL`
//!   for *any* column type — unlike `Option::<T>::None`, which only accepts one.
//! - **Result column → [`Value`]**: [`decode`] reads one column of a
//!   [`tokio_postgres::Row`] according to its Postgres type, handling `NULL`.

use bytes::BytesMut;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use rust_decimal::Decimal;
use sc_error::{Error, Result};
use sc_query::Value;
use tokio_postgres::Row as PgRow;
use tokio_postgres::types::{FromSql, IsNull, ToSql, Type};
use uuid::Uuid;

/// The boxed error type tokio-postgres' `ToSql` methods return.
type BoxError = Box<dyn std::error::Error + Sync + Send>;

/// A [`Value`] borrowed for use as a SQL bind parameter.
///
/// Encoding dispatches on the variant to the underlying type's own `ToSql`, so
/// each non-null value is type-checked against the target column exactly as that
/// type would be. `Null` short-circuits to a typeless SQL `NULL`.
#[derive(Debug)]
pub struct PgParam<'a>(pub &'a Value);

impl PgParam<'_> {
    fn encode(&self, ty: &Type, out: &mut BytesMut) -> std::result::Result<IsNull, BoxError> {
        match self.0 {
            // A NULL is valid for any column type, so bypass the per-type
            // `accepts` gate that the concrete `ToSql` impls apply.
            Value::Null => Ok(IsNull::Yes),
            Value::Bool(v) => v.to_sql_checked(ty, out),
            // An integer literal reaches a placeholder whose inferred type is
            // whatever the surrounding expression wants: it may be a narrower
            // integer (`int2`/`int4`) or `numeric` — the latter is common when a
            // formula compares against an aggregate (`sum(x) > 100`), since
            // `sum` widens to `numeric`. `i64::to_sql` only accepts `int8`, so
            // coerce to the target rather than error the whole query.
            Value::Int(v) => match *ty {
                Type::INT2 => (*v as i16).to_sql_checked(ty, out),
                Type::INT4 => (*v as i32).to_sql_checked(ty, out),
                Type::NUMERIC => Decimal::from(*v).to_sql_checked(ty, out),
                // `price > 100000` on a `double precision` column: Postgres
                // infers the placeholder from the column, and a whole-number
                // literal is still a number there.
                Type::FLOAT8 => (*v as f64).to_sql_checked(ty, out),
                Type::FLOAT4 => (*v as f32).to_sql_checked(ty, out),
                _ => v.to_sql_checked(ty, out),
            },
            Value::Float(v) => match *ty {
                Type::NUMERIC => Decimal::try_from(*v)
                    .map_err(BoxError::from)
                    .and_then(|d| d.to_sql_checked(ty, out)),
                Type::FLOAT4 => (*v as f32).to_sql_checked(ty, out),
                _ => v.to_sql_checked(ty, out),
            },
            // A geometry travels as GeoJSON and PostGIS takes EWKB (see
            // `crate::geometry`); a form may send the object's text.
            Value::Json(v) if crate::geometry::is_geometry_type(ty.name()) => {
                encode_geometry(v, out)
            }
            // A vector travels as a JSON array and pgvector takes its own
            // binary form; a form may send the array's text.
            Value::Json(v) if ty.name() == VECTOR_TYPE => encode_vector(v, out),
            Value::Text(v) if ty.name() == VECTOR_TYPE => {
                let json: serde_json::Value = serde_json::from_str(v)
                    .map_err(|_| BoxError::from(format!("{v:?} is not a vector")))?;
                encode_vector(&json, out)
            }
            Value::Text(v) if crate::geometry::is_geometry_type(ty.name()) => {
                let json: serde_json::Value = serde_json::from_str(v)
                    .map_err(|_| BoxError::from(format!("{v:?} is not a GeoJSON geometry")))?;
                encode_geometry(&json, out)
            }
            // A formula's string literal compared with a date, a time, an
            // instant or a UUID (`sold_on >= "2024-02-01"`, a dashboard's
            // date range): Postgres infers the placeholder from the column,
            // and SQLite compares the same text with what it stores.
            Value::Text(v) if *ty == Type::DATE => {
                parse_text(v, ty, parse_date)?.to_sql_checked(ty, out)
            }
            Value::Text(v) if *ty == Type::TIMESTAMPTZ => {
                parse_text(v, ty, parse_instant)?.to_sql_checked(ty, out)
            }
            Value::Text(v) if *ty == Type::TIMESTAMP => {
                parse_text(v, ty, |s| parse_instant(s).map(|t| t.naive_utc()))?
                    .to_sql_checked(ty, out)
            }
            Value::Text(v) if *ty == Type::TIME => {
                parse_text(v, ty, |s| NaiveTime::parse_from_str(s, "%H:%M:%S%.f").ok())?
                    .to_sql_checked(ty, out)
            }
            Value::Text(v) if *ty == Type::UUID => {
                parse_text(v, ty, |s| Uuid::parse_str(s).ok())?.to_sql_checked(ty, out)
            }
            // A code body's `db.sql("… where account = $1::bigint", ["10"])`:
            // the cast in the SQL decides the placeholder's type, and Postgres
            // reads the same text as an untyped literal there, so it is read
            // the same way here rather than refused for its JavaScript type.
            Value::Text(v) if *ty == Type::INT2 => {
                parse_text(v, ty, |s| s.parse::<i16>().ok())?.to_sql_checked(ty, out)
            }
            Value::Text(v) if *ty == Type::INT4 => {
                parse_text(v, ty, |s| s.parse::<i32>().ok())?.to_sql_checked(ty, out)
            }
            Value::Text(v) if *ty == Type::INT8 => {
                parse_text(v, ty, |s| s.parse::<i64>().ok())?.to_sql_checked(ty, out)
            }
            Value::Text(v) if *ty == Type::FLOAT4 => {
                parse_text(v, ty, |s| s.parse::<f32>().ok())?.to_sql_checked(ty, out)
            }
            Value::Text(v) if *ty == Type::FLOAT8 => {
                parse_text(v, ty, |s| s.parse::<f64>().ok())?.to_sql_checked(ty, out)
            }
            Value::Text(v) if *ty == Type::NUMERIC => parse_text(v, ty, |s| {
                s.parse::<Decimal>()
                    .ok()
                    .or_else(|| Decimal::from_scientific(s).ok())
            })?
            .to_sql_checked(ty, out),
            Value::Text(v) if *ty == Type::BOOL => {
                parse_text(v, ty, parse_bool)?.to_sql_checked(ty, out)
            }
            Value::Text(v) => v.to_sql_checked(ty, out),
            Value::Bytes(v) => v.to_sql_checked(ty, out),
            Value::Json(v) => v.to_sql_checked(ty, out),
            Value::Uuid(v) => v.to_sql_checked(ty, out),
            Value::Date(v) => v.to_sql_checked(ty, out),
            Value::Time(v) => v.to_sql_checked(ty, out),
            Value::Timestamp(v) => v.to_sql_checked(ty, out),
            Value::Decimal(v) => v.to_sql_checked(ty, out),
        }
    }
}

/// Text read as the value a placeholder of type `ty` takes, or the error
/// naming both.
fn parse_text<T>(
    text: &str,
    ty: &Type,
    parse: impl Fn(&str) -> Option<T>,
) -> std::result::Result<T, BoxError> {
    parse(text.trim()).ok_or_else(|| BoxError::from(format!("{text:?} is not a {}", ty.name())))
}

/// A boolean in the spellings Postgres accepts for one.
fn parse_bool(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "t" | "true" | "y" | "yes" | "on" | "1" => Some(true),
        "f" | "false" | "n" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// A date, or the date of an instant written in full.
fn parse_date(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .or_else(|| parse_instant(text).map(|t| t.date_naive()))
}

/// An instant: RFC 3339, a date and time without a zone (read as UTC), or a
/// date alone (its midnight, UTC).
fn parse_instant(text: &str) -> Option<DateTime<Utc>> {
    if let Ok(t) = DateTime::parse_from_rfc3339(text) {
        return Some(t.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, format) {
            return Some(naive.and_utc());
        }
    }
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|naive| naive.and_utc())
}

/// A GeoJSON geometry as the EWKB a PostGIS parameter takes.
fn encode_geometry(
    json: &serde_json::Value,
    out: &mut BytesMut,
) -> std::result::Result<IsNull, BoxError> {
    let bytes = crate::geometry::geojson_to_ewkb(json).map_err(BoxError::from)?;
    out.extend_from_slice(&bytes);
    Ok(IsNull::No)
}

/// pgvector's type name, which is all a parameter or column of it says about
/// itself: an extension's type has no fixed OID.
pub(crate) const VECTOR_TYPE: &str = "vector";

/// A JSON array of numbers as pgvector's binary form: the dimension count and
/// an unused word, as 16-bit integers, then each element as a big-endian
/// `f32` (pgvector's `vector_recv`).
fn encode_vector(
    json: &serde_json::Value,
    out: &mut BytesMut,
) -> std::result::Result<IsNull, BoxError> {
    let serde_json::Value::Array(items) = json else {
        return Err(BoxError::from(format!(
            "a vector is an array of numbers, not {json}"
        )));
    };
    let dimensions = i16::try_from(items.len())
        .map_err(|_| BoxError::from(format!("a vector of {} dimensions", items.len())))?;
    out.extend_from_slice(&dimensions.to_be_bytes());
    out.extend_from_slice(&0_i16.to_be_bytes());
    for item in items {
        let x = item
            .as_f64()
            .map(|x| x as f32)
            .filter(|x| x.is_finite())
            .ok_or_else(|| BoxError::from(format!("{item} is not a number of a vector")))?;
        out.extend_from_slice(&x.to_be_bytes());
    }
    Ok(IsNull::No)
}

/// pgvector's binary form as the JSON array a vector is above the driver.
fn decode_vector(raw: &[u8]) -> std::result::Result<serde_json::Value, String> {
    let header = raw.get(..4).ok_or("a vector shorter than its header")?;
    let dimensions = i16::from_be_bytes([header[0], header[1]]);
    let body = &raw[4..];
    if dimensions < 0 || body.len() != dimensions as usize * 4 {
        return Err(format!(
            "a vector of {dimensions} dimensions in {} bytes",
            body.len()
        ));
    }
    Ok(serde_json::Value::Array(
        body.chunks_exact(4)
            .map(|c| {
                let x = f32::from_be_bytes([c[0], c[1], c[2], c[3]]);
                // The shortest decimal that reads back as the same `f32`, so
                // 0.1 written is 0.1 read.
                format!("{x}")
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map_or(serde_json::Value::Null, serde_json::Value::Number)
            })
            .collect(),
    ))
}

/// A pgvector value's raw binary form, for [`decode`] to convert.
struct RawVector<'a>(&'a [u8]);

impl<'a> FromSql<'a> for RawVector<'a> {
    fn from_sql(_ty: &Type, raw: &'a [u8]) -> std::result::Result<Self, BoxError> {
        Ok(RawVector(raw))
    }

    fn accepts(ty: &Type) -> bool {
        ty.name() == VECTOR_TYPE
    }
}

/// A PostGIS value's raw binary form, for [`decode`] to convert.
struct RawGeometry<'a>(&'a [u8]);

impl<'a> FromSql<'a> for RawGeometry<'a> {
    fn from_sql(_ty: &Type, raw: &'a [u8]) -> std::result::Result<Self, BoxError> {
        Ok(RawGeometry(raw))
    }

    fn accepts(ty: &Type) -> bool {
        crate::geometry::is_geometry_type(ty.name())
    }
}

impl ToSql for PgParam<'_> {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> std::result::Result<IsNull, BoxError>
    where
        Self: Sized,
    {
        self.encode(ty, out)
    }

    // We override `to_sql_checked` to do our own per-variant dispatch, so this
    // gate is unused; accept everything and let `encode` decide.
    fn accepts(_ty: &Type) -> bool
    where
        Self: Sized,
    {
        true
    }

    fn to_sql_checked(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> std::result::Result<IsNull, BoxError> {
        self.encode(ty, out)
    }
}

/// Read one column of `row` as a [`Value`], mapping the Postgres type to a basic
/// `Value` variant and turning SQL `NULL` into [`Value::Null`].
///
/// Unmapped types are an explicit error rather than a silent coercion — this
/// milestone deliberately covers only the basic types.
pub fn decode(row: &PgRow, idx: usize) -> Result<Value> {
    let column = row
        .columns()
        .get(idx)
        .ok_or_else(|| Error::database(format!("no column at index {idx}")))?;
    let ty = column.type_();

    let value = if *ty == Type::BOOL {
        get::<Option<bool>>(row, idx)?.map_or(Value::Null, Value::Bool)
    } else if *ty == Type::INT2 {
        get::<Option<i16>>(row, idx)?.map_or(Value::Null, |n| Value::Int(n as i64))
    } else if *ty == Type::INT4 {
        get::<Option<i32>>(row, idx)?.map_or(Value::Null, |n| Value::Int(n as i64))
    } else if *ty == Type::INT8 {
        get::<Option<i64>>(row, idx)?.map_or(Value::Null, Value::Int)
    } else if *ty == Type::FLOAT4 {
        get::<Option<f32>>(row, idx)?.map_or(Value::Null, |n| Value::Float(n as f64))
    } else if *ty == Type::FLOAT8 {
        get::<Option<f64>>(row, idx)?.map_or(Value::Null, Value::Float)
    } else if *ty == Type::TEXT || *ty == Type::VARCHAR || *ty == Type::BPCHAR || *ty == Type::NAME
    {
        get::<Option<String>>(row, idx)?.map_or(Value::Null, Value::Text)
    } else if *ty == Type::BYTEA {
        get::<Option<Vec<u8>>>(row, idx)?.map_or(Value::Null, Value::Bytes)
    } else if *ty == Type::UUID {
        get::<Option<Uuid>>(row, idx)?.map_or(Value::Null, Value::Uuid)
    } else if *ty == Type::JSON || *ty == Type::JSONB {
        get::<Option<serde_json::Value>>(row, idx)?.map_or(Value::Null, Value::Json)
    } else if *ty == Type::DATE {
        get::<Option<NaiveDate>>(row, idx)?.map_or(Value::Null, Value::Date)
    } else if *ty == Type::TIME {
        get::<Option<NaiveTime>>(row, idx)?.map_or(Value::Null, Value::Time)
    } else if *ty == Type::TIMESTAMPTZ {
        get::<Option<DateTime<Utc>>>(row, idx)?.map_or(Value::Null, Value::Timestamp)
    } else if *ty == Type::TIMESTAMP {
        // No zone in the column; interpret the naive timestamp as UTC.
        get::<Option<NaiveDateTime>>(row, idx)?.map_or(Value::Null, |n| {
            Value::Timestamp(DateTime::<Utc>::from_naive_utc_and_offset(n, Utc))
        })
    } else if *ty == Type::NUMERIC {
        get::<Option<Decimal>>(row, idx)?.map_or(Value::Null, Value::Decimal)
    } else if crate::geometry::is_geometry_type(ty.name()) {
        match get::<Option<RawGeometry<'_>>>(row, idx)? {
            None => Value::Null,
            Some(RawGeometry(bytes)) => Value::Json(
                crate::geometry::ewkb_to_geojson(bytes)
                    .map_err(|e| Error::database(format!("column `{}`: {e}", column.name())))?,
            ),
        }
    } else if ty.name() == VECTOR_TYPE {
        match get::<Option<RawVector<'_>>>(row, idx)? {
            None => Value::Null,
            Some(RawVector(bytes)) => Value::Json(
                decode_vector(bytes)
                    .map_err(|e| Error::database(format!("column `{}`: {e}", column.name())))?,
            ),
        }
    } else {
        return Err(Error::database(format!(
            "unsupported column type `{ty}` for column `{}` (basic types only this milestone)",
            column.name()
        )));
    };
    Ok(value)
}

/// The Postgres type a backend type *name* refers to — the inverse of the
/// `Type::name()` this crate reports, and what
/// [`describe`](sc_db::DatabaseDriver::describe) types a prepared statement's
/// parameters with.
///
/// The names accepted are the ones `BasicType::sql_type()` produces plus the
/// aliases `BasicType::from_sql_type` recognises, so a type that survives a
/// round trip through the type layer survives this too. An unrecognised name is
/// an error rather than a guess: preparing a statement with the wrong parameter
/// type would either fail obscurely later or, worse, succeed and coerce.
pub fn pg_type(sql_type: &str) -> Result<Type> {
    Ok(match sql_type.trim().to_ascii_lowercase().as_str() {
        "bool" | "boolean" => Type::BOOL,
        "int2" | "smallint" => Type::INT2,
        "int4" | "integer" => Type::INT4,
        "int8" | "bigint" => Type::INT8,
        "float4" | "real" => Type::FLOAT4,
        "float8" | "double precision" => Type::FLOAT8,
        "numeric" | "decimal" => Type::NUMERIC,
        "text" => Type::TEXT,
        "varchar" | "character varying" => Type::VARCHAR,
        "bpchar" | "char" | "character" => Type::BPCHAR,
        "name" => Type::NAME,
        "bytea" => Type::BYTEA,
        "json" => Type::JSON,
        "jsonb" => Type::JSONB,
        "uuid" => Type::UUID,
        "date" => Type::DATE,
        "time" | "time without time zone" => Type::TIME,
        "timestamp" | "timestamp without time zone" => Type::TIMESTAMP,
        "timestamptz" | "timestamp with time zone" => Type::TIMESTAMPTZ,
        other => {
            return Err(Error::database(format!(
                "`{other}` is not a Postgres type this driver can bind a parameter as"
            )));
        }
    })
}

/// Read a typed value from a column, converting the driver error into ours.
fn get<'a, T: FromSql<'a>>(row: &'a PgRow, idx: usize) -> Result<T> {
    row.try_get::<usize, T>(idx)
        .map_err(|e| Error::database(format!("decode column {idx}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An integer literal whose placeholder Postgres infers as `numeric`
    /// (comparing against `sum(…)`, say) must encode, not error — the whole
    /// point of the per-target coercion.
    #[test]
    fn an_int_encodes_into_a_numeric_placeholder() {
        let mut out = BytesMut::new();
        // Plain `i64::to_sql` refuses NUMERIC; `PgParam` coerces to Decimal.
        assert!(
            PgParam(&Value::Int(100))
                .to_sql_checked(&Type::NUMERIC, &mut out)
                .is_ok()
        );
        assert!(
            PgParam(&Value::Int(7))
                .to_sql_checked(&Type::INT4, &mut out)
                .is_ok()
        );
        assert!(
            PgParam(&Value::Int(7))
                .to_sql_checked(&Type::INT2, &mut out)
                .is_ok()
        );
        assert!(
            PgParam(&Value::Float(2.5))
                .to_sql_checked(&Type::NUMERIC, &mut out)
                .is_ok()
        );
        // The int8 path still works.
        assert!(
            PgParam(&Value::Int(9))
                .to_sql_checked(&Type::INT8, &mut out)
                .is_ok()
        );
        // `price > 100000` on a double-precision column: a whole number into
        // a float placeholder (a dataset's Filter, analytics TODO A1.3).
        assert!(
            PgParam(&Value::Int(100_000))
                .to_sql_checked(&Type::FLOAT8, &mut out)
                .is_ok()
        );
        assert!(
            PgParam(&Value::Float(1.5))
                .to_sql_checked(&Type::FLOAT4, &mut out)
                .is_ok()
        );
    }

    /// A formula's string literal compared with a date, an instant, a time
    /// or a UUID column: `sold_on >= "2024-02-01"` (a dashboard's date range,
    /// analytics TODO A6.3).
    #[test]
    fn text_encodes_into_temporal_and_uuid_placeholders() {
        let mut out = BytesMut::new();
        let text = |s: &str| Value::Text(s.to_owned());
        for (value, ty) in [
            (text("2024-02-01"), Type::DATE),
            (text("2024-02-01T12:00:00.000Z"), Type::DATE),
            (text("2024-02-01T12:00:00.000Z"), Type::TIMESTAMPTZ),
            (text("2024-02-01"), Type::TIMESTAMPTZ),
            (text("2024-02-01 12:00:00"), Type::TIMESTAMP),
            (text("12:30:00"), Type::TIME),
            (text("67e55044-10b1-426f-9247-bb680e5fe0c8"), Type::UUID),
        ] {
            assert!(
                PgParam(&value).to_sql_checked(&ty, &mut out).is_ok(),
                "{value:?} into {ty}"
            );
        }
        let Err(err) = PgParam(&text("soon")).to_sql_checked(&Type::DATE, &mut out) else {
            panic!("\"soon\" is not a date");
        };
        assert!(err.to_string().contains("\"soon\" is not a date"), "{err}");
        assert_eq!(
            parse_instant("2024-02-01").map(|t| t.to_rfc3339()),
            Some("2024-02-01T00:00:00+00:00".to_owned())
        );
    }

    /// `db.sql("… where account = $1::bigint", ["10"])`: text into a
    /// placeholder the SQL cast to a number or a boolean is read as Postgres
    /// reads an untyped literal there, not refused for being a string.
    #[test]
    fn text_encodes_into_numeric_and_boolean_placeholders() {
        let mut out = BytesMut::new();
        let text = |s: &str| Value::Text(s.to_owned());
        for (value, ty) in [
            (text("10"), Type::INT8),
            (text(" -7 "), Type::INT4),
            (text("3"), Type::INT2),
            (text("2.5"), Type::FLOAT8),
            (text("2.5"), Type::FLOAT4),
            (text("12.50"), Type::NUMERIC),
            (text("1e3"), Type::NUMERIC),
            (text("true"), Type::BOOL),
            (text("f"), Type::BOOL),
        ] {
            assert!(
                PgParam(&value).to_sql_checked(&ty, &mut out).is_ok(),
                "{value:?} into {ty}"
            );
        }
        let mut out = BytesMut::new();
        PgParam(&text("10"))
            .to_sql_checked(&Type::INT8, &mut out)
            .unwrap();
        assert_eq!(&out[..], &10i64.to_be_bytes());
        let Err(err) = PgParam(&text("ten")).to_sql_checked(&Type::INT8, &mut out) else {
            panic!("\"ten\" is not an int8");
        };
        assert!(err.to_string().contains("\"ten\" is not a int8"), "{err}");
        assert!(
            PgParam(&text("70000"))
                .to_sql_checked(&Type::INT2, &mut out)
                .is_err()
        );
    }
}
