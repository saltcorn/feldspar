//! The **vector** field type (TODO-agent-skills AS2.1).
//!
//! A vector is a fixed number of 32-bit floats: an embedding of a row's text,
//! which a read can order its rows by the cosine distance to (`nearest`). Its
//! number of dimensions is part of the column's type, `vector(1536)`, because
//! two vectors of different lengths have no distance and pgvector refuses a
//! value of the wrong length.
//!
//! Above the database driver a vector is a **JSON array of numbers** carried
//! in a [`Value::Json`], as a geometry is a GeoJSON object: the REST and
//! GraphQL wire shape, what a formula reads, and what a client writes. Each
//! driver converts at its own edge:
//!
//! - **Postgres** stores pgvector's `vector(n)`. The driver encodes and decodes
//!   pgvector's binary form (a dimension count, then big-endian `f32`s).
//! - **SQLite** stores a blob of little-endian `f32`s ([`to_blob`]) in a column
//!   declared `vector(n)`, which is how the driver knows to read the blob back
//!   as an array. A write reaches the blob through `CAST(? AS vector(n))`, which
//!   the SQLite dialect renders as the `_fd_vector(?, n)` function the driver
//!   registers.
//!
//! [`vector_value`] checks a value: an array of finite numbers of the field's
//! length. A value of the wrong length is refused with both lengths, because
//! "not a valid vector" names nothing to fix.

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

use sc_error::{Error, Result};
use sc_query::Value;
use serde_json::Value as Json;

use crate::{BasicType, FormField};

/// The most dimensions a vector field may have: pgvector's own limit.
pub const MAX_DIMENSIONS: u32 = 16_000;

/// The attribute a field created as plain `vector` gives its dimensions in.
pub const ATTR_DIMENSIONS: &str = "dimensions";
/// The attribute holding the formula a vector field is embedded from
/// (AS2.5): evaluated over the row, its text is embedded on every write that
/// changes what it reads.
pub const ATTR_EMBED_FROM: &str = "embed_from";
/// The LLM provider whose embedding endpoint computes the field (blank for
/// the installation's default provider).
pub const ATTR_EMBED_PROVIDER: &str = "embed_provider";
/// The embedding model the provider is asked for (`text-embedding-3-small`).
pub const ATTR_EMBED_MODEL: &str = "embed_model";

/// The name the field-type picker offers a vector as. The dimensions are an
/// attribute ([`ATTR_DIMENSIONS`]); the field's type is then `vector(n)`.
pub const VECTOR_TYPE_NAME: &str = "vector";

/// The attributes a vector field takes, as the form the admin UI renders when
/// `vector` is picked.
pub fn vector_attributes() -> Vec<FormField> {
    vec![
        FormField::new(ATTR_DIMENSIONS, BasicType::Int)
            .label("Dimensions (the embedding model's output size, such as 1536)")
            .required(),
        FormField::new(ATTR_EMBED_FROM, BasicType::Text)
            .label("Embed from (a formula over the row, such as title + \" \" + body)"),
        FormField::new(ATTR_EMBED_PROVIDER, BasicType::Text)
            .label("Embedding provider (blank for the default LLM provider)"),
        FormField::new(ATTR_EMBED_MODEL, BasicType::Text)
            .label("Embedding model (such as text-embedding-3-small)"),
    ]
}

/// The interned `vector(n)` spelling, which is both the type's name and its SQL
/// type. [`BasicType::name`] and [`BasicType::sql_type`] lend out a `&str`, and
/// a field's dimensions are only known at run time, so each spelling is made
/// once and kept: there are as many as there are distinct dimension counts in
/// use, which is a handful.
pub(crate) fn spelling(dimensions: u32) -> &'static str {
    static SPELLINGS: LazyLock<Mutex<BTreeMap<u32, &'static str>>> =
        LazyLock::new(|| Mutex::new(BTreeMap::new()));
    if dimensions == 0 {
        return VECTOR_TYPE_NAME;
    }
    let mut spellings = match SPELLINGS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    spellings
        .entry(dimensions)
        .or_insert_with(|| Box::leak(format!("vector({dimensions})").into_boxed_str()))
}

/// The dimensions a SQL type or field type name spells: `vector(3)` (any case,
/// spaces allowed) is `Some(3)`, and the bare `vector` of the type picker is
/// `Some(0)`. `None` for anything else.
pub fn of_sql_type(sql_type: &str) -> Option<u32> {
    let compact: String = sql_type
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    if compact == VECTOR_TYPE_NAME {
        return Some(0);
    }
    let n = compact.strip_prefix("vector(")?.strip_suffix(')')?;
    n.parse::<u32>().ok().filter(|n| *n > 0)
}

/// Refuse a dimension count a column cannot have.
pub fn check_dimensions(dimensions: u32) -> Result<()> {
    if dimensions == 0 || dimensions > MAX_DIMENSIONS {
        return Err(Error::invalid(format!(
            "a vector field has between 1 and {MAX_DIMENSIONS} dimensions, not {dimensions}"
        )));
    }
    Ok(())
}

/// The floats of a JSON array of numbers, or the sentence saying why it is not
/// one — any length.
pub fn floats(json: &Json) -> Result<Vec<f32>> {
    let Json::Array(items) = json else {
        return Err(Error::invalid(format!(
            "a vector is an array of numbers, not {}",
            describe(json)
        )));
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            item.as_f64()
                .map(|x| x as f32)
                .filter(|x| x.is_finite())
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "a vector is an array of finite numbers, and element {i} is {}",
                        describe(item)
                    ))
                })
        })
        .collect()
}

/// A JSON value as the [`Value`] a `vector(dimensions)` column holds: an array
/// of exactly `dimensions` finite numbers. A form or a CSV cell sends the
/// array's text, which is read too. `dimensions` of 0 takes any length.
pub fn vector_value(dimensions: u32, json: &Json) -> Result<Value> {
    let parsed;
    let json = match json {
        Json::String(s) => {
            parsed = serde_json::from_str::<Json>(s).map_err(|_| {
                Error::invalid(format!(
                    "{s:?} is not a vector: a vector is written as an array of numbers, \
                     such as [0.12, -0.5, 0.33]"
                ))
            })?;
            &parsed
        }
        other => other,
    };
    let values = floats(json)?;
    check_length(dimensions, values.len())?;
    Ok(Value::Json(to_json(&values)))
}

/// Refuse a vector of `len` numbers for a field of `dimensions`.
pub fn check_length(dimensions: u32, len: usize) -> Result<()> {
    if dimensions != 0 && len != dimensions as usize {
        return Err(Error::invalid(format!(
            "this vector has {len} dimensions, and the field holds vectors of {dimensions}"
        )));
    }
    Ok(())
}

/// Floats as the JSON array a vector is on the wire. Each is written as the
/// shortest decimal that reads back as the same `f32`, so `0.1` stored is
/// `0.1` read rather than `0.10000000149011612`.
pub fn to_json(values: &[f32]) -> Json {
    Json::Array(
        values
            .iter()
            .map(|x| {
                format!("{x}")
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map_or(Json::Null, Json::Number)
            })
            .collect(),
    )
}

/// The little-endian `f32` blob a vector is stored as on SQLite.
pub fn to_blob(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// The floats of a SQLite vector blob, or `None` when its length is not a
/// whole number of `f32`s.
pub fn from_blob(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.len() % 4 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

/// The cosine distance between two vectors: `1 - cos θ`, from 0 (the same
/// direction) to 2 (opposite), as pgvector's `<=>` computes it. `None` when the
/// lengths differ; NaN when either is all zeros, as pgvector's is.
pub fn cosine_distance(a: &[f32], b: &[f32]) -> Option<f64> {
    if a.len() != b.len() {
        return None;
    }
    let (mut dot, mut na, mut nb) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    Some(1.0 - dot / (na.sqrt() * nb.sqrt()))
}

fn describe(json: &Json) -> String {
    match json {
        Json::Null => "null".to_owned(),
        Json::Bool(_) => "a boolean".to_owned(),
        Json::Number(_) => "a single number".to_owned(),
        Json::String(s) => format!("the text {s:?}"),
        Json::Array(_) => "an array".to_owned(),
        Json::Object(_) => "an object".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_vector_type_is_spelled_with_its_dimensions() {
        assert_eq!(of_sql_type("vector(3)"), Some(3));
        assert_eq!(of_sql_type(" VECTOR ( 1536 ) "), Some(1536));
        assert_eq!(of_sql_type("vector"), Some(0));
        assert_eq!(of_sql_type("vector(0)"), None);
        assert_eq!(of_sql_type("vector(x)"), None);
        assert_eq!(of_sql_type("halfvec(3)"), None);
        assert_eq!(spelling(3), "vector(3)");
        assert!(std::ptr::eq(spelling(3), spelling(3)), "interned once");
        assert!(check_dimensions(0).is_err());
        assert!(check_dimensions(16_001).is_err());
        assert!(check_dimensions(1536).is_ok());
    }

    #[test]
    fn a_vector_field_type_round_trips_and_validates_its_length() {
        let ty = BasicType::from_name("vector(3)");
        assert_eq!(ty, BasicType::Vector(3));
        assert_eq!(ty.name(), "vector(3)");
        assert_eq!(ty.sql_type(), "vector(3)");
        assert_eq!(BasicType::from_sql_type(ty.sql_type()), ty);
        assert_eq!(BasicType::from_name("vector"), BasicType::Vector(0));
        assert!(ty.accepts(&Value::Json(json!([1, 2, 3]))));
        assert!(!ty.accepts(&Value::Json(json!([1, 2]))));
        assert!(!ty.accepts(&Value::Text("[1,2,3]".into())));
        let err = ty.validate(&Value::Json(json!([1]))).unwrap_err();
        assert!(err.to_string().contains("1 dimensions"), "{err}");
        assert!(ty.accepts_json(&json!([0.5, 0.5, 0.5])));
        assert!(ty.accepts_json(&json!(null)));
        assert_eq!(
            crate::catchall::parse(&ty, "[1, 2, 3]").unwrap(),
            Value::Json(json!([1.0, 2.0, 3.0]))
        );
    }

    #[test]
    fn a_value_is_an_array_of_numbers_of_the_fields_length() {
        assert_eq!(
            vector_value(3, &json!([0.1, 2, -3.5])).unwrap(),
            Value::Json(json!([0.1, 2.0, -3.5]))
        );
        // A form sends the array's text.
        assert_eq!(
            vector_value(2, &json!("[1, 0]")).unwrap(),
            Value::Json(json!([1.0, 0.0]))
        );
        let err = vector_value(3, &json!([1, 2])).unwrap_err().to_string();
        assert!(
            err.contains("has 2 dimensions") && err.contains("of 3"),
            "{err}"
        );
        let err = vector_value(2, &json!([1, "x"])).unwrap_err().to_string();
        assert!(err.contains("element 1"), "{err}");
        let err = vector_value(2, &json!({"a": 1})).unwrap_err().to_string();
        assert!(err.contains("not an object"), "{err}");
        assert!(vector_value(2, &json!("nope")).is_err());
    }

    #[test]
    fn floats_round_trip_through_the_blob_and_read_back_short() {
        let v = [0.1_f32, -2.5, 3.0e-7];
        let blob = to_blob(&v);
        assert_eq!(blob.len(), 12);
        assert_eq!(from_blob(&blob).unwrap(), v.to_vec());
        assert!(from_blob(&[0, 1, 2]).is_none());
        assert_eq!(to_json(&v), json!([0.1, -2.5, 3.0e-7]));
    }

    #[test]
    fn cosine_distance_is_pgvectors() {
        assert_eq!(cosine_distance(&[1.0, 0.0], &[1.0, 0.0]), Some(0.0));
        assert!((cosine_distance(&[1.0, 0.0], &[0.0, 1.0]).unwrap() - 1.0).abs() < 1e-12);
        assert!((cosine_distance(&[1.0, 0.0], &[-1.0, 0.0]).unwrap() - 2.0).abs() < 1e-12);
        assert_eq!(cosine_distance(&[1.0], &[1.0, 0.0]), None);
        assert!(cosine_distance(&[0.0, 0.0], &[1.0, 0.0]).unwrap().is_nan());
    }
}
