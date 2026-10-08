//! The catch-all display/edit path.
//!
//! The full [`FieldView`] system — per-type editors implemented as React
//! (TypeScript) components — is deferred to post-MVP (technical design §6.3; this
//! milestone ships "no rich types … everything is basic"). What the MVP server
//! *does* need is a single, type-directed way to turn any [`Value`] into
//! displayable text and to parse a submitted string back into a [`Value`] of the
//! right [`BasicType`]. That is the catch-all fieldview reduced to its two
//! essential operations, over plain strings.
//!
//! - [`display`] renders a value as text for a table cell or an input's current
//!   value. It returns plain text, not HTML, so escaping is the caller's concern.
//! - [`parse`] converts one submitted string into a [`Value`], validating it
//!   against the target [`BasicType`]. An empty string parses to [`Value::Null`]
//!   so that clearing a field is expressible.

use std::str::FromStr;

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use rust_decimal::Decimal;
use sc_error::{Error, Result};
use sc_query::Value;
use uuid::Uuid;

use crate::BasicType;

/// Render a value as plain text for display or as an editor's current value.
///
/// [`Value::Null`] renders as the empty string. Bytes render as lowercase hex so
/// the representation is lossless and round-trips through [`parse`]. The output
/// is plain *text*, not HTML — callers escape it for their target medium.
pub fn display(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Decimal(d) => d.to_string(),
        Value::Text(s) => s.clone(),
        Value::Bytes(b) => to_hex(b),
        Value::Json(j) => j.to_string(),
        Value::Uuid(u) => u.to_string(),
        Value::Date(d) => d.format("%Y-%m-%d").to_string(),
        Value::Time(t) => t.format("%H:%M:%S").to_string(),
        Value::Timestamp(ts) => ts.to_rfc3339(),
    }
}

/// Parse a submitted form string into a [`Value`] of the given type.
///
/// A whitespace-only or empty string becomes [`Value::Null`] (clearing the
/// field). Otherwise the string is parsed according to `ty`; a malformed input
/// is an [`Error::Invalid`] naming the type, never a silent default.
pub fn parse(ty: &BasicType, input: &str) -> Result<Value> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Value::Null);
    }
    let value = match ty {
        BasicType::Bool => Value::Bool(parse_bool(trimmed)?),
        BasicType::Int => Value::Int(i64::from_str(trimmed).map_err(|e| bad(ty, input, e))?),
        BasicType::Float => Value::Float(f64::from_str(trimmed).map_err(|e| bad(ty, input, e))?),
        BasicType::Decimal => {
            Value::Decimal(Decimal::from_str(trimmed).map_err(|e| bad(ty, input, e))?)
        }
        // Text keeps the raw, untrimmed input: leading/trailing spaces may be
        // meaningful in a text field.
        BasicType::Text | BasicType::Other(_) => Value::Text(input.to_owned()),
        BasicType::Bytes => Value::Bytes(from_hex(trimmed).map_err(|e| bad(ty, input, e))?),
        BasicType::Json => {
            Value::Json(serde_json::Value::from_str(trimmed).map_err(|e| bad(ty, input, e))?)
        }
        BasicType::Geometry(_) => {
            crate::json_to_value(ty, &serde_json::Value::String(trimmed.to_owned()))?
        }
        BasicType::Uuid => Value::Uuid(Uuid::from_str(trimmed).map_err(|e| bad(ty, input, e))?),
        BasicType::Date => Value::Date(
            NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").map_err(|e| bad(ty, input, e))?,
        ),
        BasicType::Time => Value::Time(parse_time(trimmed).map_err(|e| bad(ty, input, e))?),
        BasicType::Timestamp => Value::Timestamp(parse_timestamp(trimmed)?),
    };
    Ok(value)
}

/// Accept the usual truthy/falsey spellings an HTML form or CLI might submit.
fn parse_bool(s: &str) -> Result<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "t" | "1" | "on" | "yes" | "y" => Ok(true),
        "false" | "f" | "0" | "off" | "no" | "n" => Ok(false),
        _ => Err(Error::invalid(format!("`{s}` is not a valid boolean"))),
    }
}

/// Parse `HH:MM` or `HH:MM:SS`.
fn parse_time(s: &str) -> std::result::Result<NaiveTime, chrono::ParseError> {
    NaiveTime::parse_from_str(s, "%H:%M:%S").or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
}

/// Parse an RFC 3339 timestamp, normalising to UTC.
fn parse_timestamp(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| Error::invalid(format!("`{s}` is not a valid RFC 3339 timestamp: {e}")))
}

/// Build the standard "could not parse X as type Y" error.
fn bad(ty: &BasicType, input: &str, cause: impl std::fmt::Display) -> Error {
    Error::invalid(format!(
        "could not parse `{input}` as type `{}`: {cause}",
        ty.name()
    ))
}

/// Lowercase hex encoding, no separators.
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a String is infallible.
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Decode a lowercase/uppercase hex string, rejecting odd length or non-hex.
fn from_hex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        return Err(Error::invalid("hex input has an odd number of digits"));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_digit(bytes[i])?;
        let lo = hex_digit(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_digit(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(Error::invalid(format!(
            "`{}` is not a hex digit",
            b as char
        ))),
    }
}
