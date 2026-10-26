//! A code query's declared response type (TODO F4.1, design "Query contracts").
//!
//! A SQL query is typed by the columns the database describes; a JavaScript or
//! Python body returns whatever it returns, so its client method used to answer
//! `unknown`. `returns` is the body's author saying what it returns, in a small
//! JSON type language:
//!
//! ```text
//! T := "string" | "int" | "float" | "bool" | "json"      -- and the other wire
//!      | "uuid" | "date" | "time" | "timestamp" | …       -- type names
//!      | { "object": { name: T, … } }
//!      | { "array": T }
//!      | { "optional": T }                                -- T or null
//!      | { "union": [T, T, …] }
//! ```
//!
//! It is lowered to a [`TypeSchema`] ([`parse_returns`]), which is what the
//! generator emits `XResponse` from. It is a **contract for the client**, not a
//! filter: a production response is never checked against it (a mismatch would
//! otherwise turn an answer the client could have used into an error). A run's
//! preview checks it ([`check_returns`]) and logs a warning naming the first
//! place the value strays, so the author finds out while building.

use serde_json::Value as Json;

use crate::schema::{StructField, TypeSchema, ValueType};

/// How deeply a `returns` type may nest. A real response type is a handful of
/// levels; this bound is what keeps a pathological one from recursing the
/// stack away.
const MAX_DEPTH: usize = 32;

/// Lower a `returns` declaration to the schema the generator types the client
/// method with. The error says where in the declaration the problem is.
pub fn parse_returns(declared: &Json) -> Result<TypeSchema, String> {
    lower(declared, "returns", 0)
}

fn lower(t: &Json, at: &str, depth: usize) -> Result<TypeSchema, String> {
    if depth > MAX_DEPTH {
        return Err(format!("{at}: nested more than {MAX_DEPTH} levels deep"));
    }
    match t {
        Json::String(name) => scalar(name)
            .map(TypeSchema::value)
            .ok_or_else(|| format!("{at}: `{name}` is not a type; use one of {}", SCALARS)),
        Json::Object(map) if map.len() == 1 => {
            let Some((kind, inner)) = map.iter().next() else {
                return Err(format!("{at}: an empty object is not a type"));
            };
            match kind.as_str() {
                "object" => {
                    let Json::Object(fields) = inner else {
                        return Err(format!(
                            "{at}.object: should map each property name to its type"
                        ));
                    };
                    if fields.is_empty() {
                        return Err(format!(
                            "{at}.object: has no properties; use \"json\" for an object \
                             of any shape"
                        ));
                    }
                    let mut out = Vec::with_capacity(fields.len());
                    for (name, field) in fields {
                        if !is_property_name(name) {
                            return Err(format!(
                                "{at}.object: `{name}` is not usable as a property name; \
                                 use letters, digits and underscores"
                            ));
                        }
                        out.push(StructField::new(
                            name,
                            lower(field, &format!("{at}.object.{name}"), depth + 1)?,
                        ));
                    }
                    Ok(TypeSchema::struct_of(out))
                }
                "array" => Ok(TypeSchema::array(lower(
                    inner,
                    &format!("{at}.array"),
                    depth + 1,
                )?)),
                "optional" => {
                    let inner = lower(inner, &format!("{at}.optional"), depth + 1)?;
                    // `optional` of `optional` says nothing more.
                    Ok(match inner {
                        TypeSchema::Optional(_) => inner,
                        other => TypeSchema::optional(other),
                    })
                }
                "union" => {
                    let Json::Array(alternatives) = inner else {
                        return Err(format!("{at}.union: should be a list of types"));
                    };
                    if alternatives.len() < 2 {
                        return Err(format!("{at}.union: needs at least two alternatives"));
                    }
                    let lowered = alternatives
                        .iter()
                        .enumerate()
                        .map(|(i, a)| lower(a, &format!("{at}.union[{i}]"), depth + 1))
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(TypeSchema::union(lowered))
                }
                other => Err(format!(
                    "{at}: `{other}` is not a type constructor; use object, array, \
                     optional or union"
                )),
            }
        }
        _ => Err(format!(
            "{at}: should be a type name such as \"string\", or one of {{\"object\": …}}, \
             {{\"array\": …}}, {{\"optional\": …}}, {{\"union\": […]}}"
        )),
    }
}

/// The scalar names the language accepts, for an error message.
const SCALARS: &str = "string, int, float, bool, json, uuid, date, time, timestamp, decimal";

fn scalar(name: &str) -> Option<ValueType> {
    match name {
        "string" => Some(ValueType::Text),
        "number" => Some(ValueType::Float),
        "boolean" => Some(ValueType::Bool),
        other => ValueType::from_name(other),
    }
}

fn is_property_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Check `value` against `schema`: `None` when it conforms, or the first place
/// it does not, as `$.items[2].name: expected int, got "x"`.
///
/// An object may carry properties the type does not name — a TypeScript object
/// type does not forbid them either, and a body returning a little more than it
/// promised breaks no client.
pub fn check_returns(schema: &TypeSchema, value: &Json) -> Option<String> {
    check(schema, value, "$").err()
}

fn check(schema: &TypeSchema, value: &Json, at: &str) -> Result<(), String> {
    let mismatch = |expected: &str| {
        Err(format!(
            "{at}: expected {expected}, got {}",
            preview_value(value)
        ))
    };
    match schema {
        TypeSchema::Optional(inner) => match value {
            Json::Null => Ok(()),
            _ => check(inner, value, at),
        },
        TypeSchema::Union(alternatives) => {
            if alternatives.iter().any(|a| check(a, value, at).is_ok()) {
                Ok(())
            } else {
                mismatch(&describe(schema))
            }
        }
        TypeSchema::Array(inner) => match value {
            Json::Array(items) => items
                .iter()
                .enumerate()
                .try_for_each(|(i, item)| check(inner, item, &format!("{at}[{i}]"))),
            _ => mismatch("an array"),
        },
        TypeSchema::Struct(fields) => match value {
            Json::Object(map) => fields.iter().try_for_each(|f| {
                let at = format!("{at}.{}", f.name);
                match map.get(&f.name) {
                    Some(v) => check(&f.schema, v, &at),
                    None if matches!(f.schema, TypeSchema::Optional(_)) => Ok(()),
                    None => Err(format!("{at}: missing")),
                }
            }),
            _ => mismatch("an object"),
        },
        TypeSchema::Value(t) => {
            let ok = match t {
                ValueType::Json => true,
                ValueType::Bool => value.is_boolean(),
                ValueType::Int => value.is_i64() || value.is_u64(),
                ValueType::Float => value.is_number(),
                // Carried as a string so its precision survives; a body that
                // returns a plain number is understood just the same.
                ValueType::Decimal => value.is_string() || value.is_number(),
                ValueType::Geometry => value.is_object(),
                ValueType::Vector => value
                    .as_array()
                    .is_some_and(|items| items.iter().all(serde_json::Value::is_number)),
                ValueType::Text
                | ValueType::Bytes
                | ValueType::Uuid
                | ValueType::Date
                | ValueType::Time
                | ValueType::Timestamp => value.is_string(),
            };
            if ok { Ok(()) } else { mismatch(t.name()) }
        }
    }
}

/// A schema in the declaration's own words, for a mismatch message.
fn describe(schema: &TypeSchema) -> String {
    match schema {
        TypeSchema::Value(t) => t.name().to_owned(),
        TypeSchema::Struct(_) => "an object".to_owned(),
        TypeSchema::Array(_) => "an array".to_owned(),
        TypeSchema::Optional(inner) => format!("{} or null", describe(inner)),
        TypeSchema::Union(alternatives) => alternatives
            .iter()
            .map(describe)
            .collect::<Vec<_>>()
            .join(" or "),
    }
}

/// A value as a warning shows it: short, whatever it was.
fn preview_value(value: &Json) -> String {
    let text = value.to_string();
    if text.chars().count() <= 60 {
        text
    } else {
        format!("{}…", text.chars().take(60).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_type_language_lowers_to_schemas() {
        let t = parse_returns(&json!({ "object": {
            "id": "uuid",
            "count": "int",
            "tags": { "array": "string" },
            "note": { "optional": "string" },
            "result": { "union": ["int", { "object": { "error": "string" } }] },
        }}))
        .unwrap();
        let TypeSchema::Struct(fields) = &t else {
            panic!("an object lowers to a struct: {t:?}")
        };
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["id", "count", "tags", "note", "result"]);
        assert_eq!(fields[0].schema, TypeSchema::uuid());
        assert_eq!(fields[2].schema, TypeSchema::array(TypeSchema::text()));
        assert_eq!(fields[3].schema, TypeSchema::optional(TypeSchema::text()));
        assert!(matches!(&fields[4].schema, TypeSchema::Union(a) if a.len() == 2));
        assert_eq!(parse_returns(&json!("json")).unwrap(), TypeSchema::json());
    }

    #[test]
    fn a_bad_declaration_names_where_it_is_wrong() {
        let cases = [
            (json!("integer"), "returns: `integer` is not a type"),
            (json!({ "array": "nope" }), "returns.array: `nope`"),
            (json!({ "object": {} }), "has no properties"),
            (json!({ "object": { "a b": "int" } }), "`a b` is not usable"),
            (json!({ "union": ["int"] }), "at least two"),
            (
                json!({ "tuple": ["int"] }),
                "`tuple` is not a type constructor",
            ),
            (json!({ "array": "int", "x": 1 }), "should be a type name"),
            (json!(7), "should be a type name"),
        ];
        for (declared, expected) in cases {
            let err = parse_returns(&declared).unwrap_err();
            assert!(err.contains(expected), "{declared}: {err}");
        }
        let mut deep = json!("int");
        for _ in 0..40 {
            deep = json!({ "array": deep });
        }
        assert!(parse_returns(&deep).unwrap_err().contains("nested"));
    }

    #[test]
    fn values_are_checked_against_the_schema() {
        let t = parse_returns(&json!({ "object": {
            "count": "int",
            "items": { "array": { "object": { "name": "string" } } },
            "note": { "optional": "string" },
            "either": { "union": ["int", "string"] },
        }}))
        .unwrap();
        let good = json!({ "count": 2, "items": [{ "name": "a" }], "either": "x", "extra": 1 });
        assert_eq!(check_returns(&t, &good), None);
        let cases = [
            (
                json!({ "count": "2", "items": [], "either": 1 }),
                "$.count: expected int",
            ),
            (
                json!({ "count": 2, "items": [{ "name": 1 }], "either": 1 }),
                "$.items[0].name: expected text",
            ),
            (json!({ "items": [], "either": 1 }), "$.count: missing"),
            (
                json!({ "count": 2, "items": [], "either": true }),
                "$.either: expected int or text",
            ),
            (json!([1]), "$: expected an object"),
        ];
        for (value, expected) in cases {
            let err = check_returns(&t, &value).unwrap();
            assert!(err.starts_with(expected), "{value}: {err}");
        }
        assert_eq!(
            check_returns(&TypeSchema::optional(TypeSchema::int()), &Json::Null),
            None
        );
        assert!(check_returns(&TypeSchema::int(), &json!(1.5)).is_some());
    }
}
