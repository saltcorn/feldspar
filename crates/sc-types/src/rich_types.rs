//! The initial rich types (design §2.2): [`StringType`] and [`IntegerType`].
//!
//! Small on purpose — enough to prove the things the machinery from §2.1 must do:
//! validate a value, declare typed attributes, and constrain what may be stored
//! against them.
//!
//! - **`String`** over `text` — a `max_length`, an optional `options` list, and
//!   an optional `regex` pattern. Proves attributes drive validation several
//!   ways: a length limit, a fixed choice set, and an arbitrary pattern.
//! - **`Integer`** over `int8` — `min`/`max`, proving numeric attribute
//!   validation.
//!
//! **`File` is deliberately not here.** §2.2 resolved it as a *field kind*
//! (`DataFieldKind::File`), not a rich type: it is not a value family but a
//! reference, like `Key`, whose storage type is `text`. The admin-facing type
//! picker (§3.4) merges kinds and types into one list because that is how an
//! admin thinks, but the model keeps them apart.
//!
//! Each type stores its attribute spec as a `Vec<FormField>` built once at
//! construction, so [`attributes`](crate::RichType::attributes) can hand back a
//! borrow. The registry ([`crate::rich`]) holds one `Arc` of each.

use std::sync::Arc;

use sc_error::{Error, Result};
use sc_query::Value;

use crate::{Attrs, BasicType, FormField, RichType};

/// The rich types registered by default — what [`builtin_rich_types`] hands the
/// registry. A new built-in type is added here.
pub(crate) fn builtin_rich_types() -> Vec<Arc<dyn RichType>> {
    vec![
        Arc::new(StringType::new()),
        Arc::new(IntegerType::new()),
        Arc::new(CollabDocType),
    ]
}

/// The registry name of [`CollabDocType`].
pub const COLLAB_DOC: &str = "collab_doc";

/// Whether a field of type `ty` holds a collaborative document.
pub fn is_collab_doc(ty: &crate::TypeRef) -> bool {
    ty.as_rich().is_some_and(|r| r.name() == COLLAB_DOC)
}

/// A collaborative document (TODO.md "Live updates" L5.1): Yjs's binary state,
/// stored as `bytea` (a `BLOB` on SQLite), edited only through a `document`
/// stream's room or `live.document` in a code body.
///
/// The type is a promise about who writes the value, and the rest of the
/// system keeps it: the column is **left out of every API read** (not sent as
/// null — the generated row types omit it too), **refused as a write target**
/// by REST, the `insert_row` and `update_rows` actions and the admin row
/// editor, and cannot be named by a filter, an ordering or a formula. A
/// whole-value overwrite would discard every edit made concurrently with it;
/// only a CRDT update merges.
#[derive(Debug, Clone, Copy, Default)]
pub struct CollabDocType;

impl RichType for CollabDocType {
    fn name(&self) -> &str {
        COLLAB_DOC
    }

    fn attributes(&self) -> &[FormField] {
        &[]
    }

    fn validate(&self, value: &Value, _attrs: &Attrs) -> Result<()> {
        match value {
            Value::Null | Value::Bytes(_) => Ok(()),
            other => Err(Error::invalid(format!(
                "a collaborative document is stored as bytes, got a value of kind `{}`",
                other.kind()
            ))),
        }
    }

    fn sql_types(&self) -> &[&str] {
        const TYPES: &[&str] = &["bytea"];
        TYPES
    }
}

/// The `options` attribute of [`StringType`].
const ATTR_OPTIONS: &str = "options";
/// The `max_length` attribute of [`StringType`].
const ATTR_MAX_LENGTH: &str = "max_length";
/// The `regex` attribute of [`StringType`].
const ATTR_REGEX: &str = "regex";
/// The `min` attribute of [`IntegerType`].
const ATTR_MIN: &str = "min";
/// The `max` attribute of [`IntegerType`].
const ATTR_MAX: &str = "max";

/// A text value with an optional maximum length, an optional fixed set of allowed
/// values, and an optional pattern the whole value must match (design §2.2).
///
/// Configuring `options` turns the field into a select over that list; `regex`
/// constrains free text; all three attributes are optional, so a bare `String`
/// is "text, validated as text".
pub struct StringType {
    attributes: Vec<FormField>,
}

impl StringType {
    /// The registry name.
    pub const NAME: &'static str = "string";

    /// A `String` type with its attribute spec built.
    pub fn new() -> StringType {
        StringType {
            attributes: vec![
                FormField::new(ATTR_MAX_LENGTH, BasicType::Int).label("Maximum length"),
                // A JSON array of the allowed values. Any JSON shape passes the
                // structural check; `validate` reads it as an array of choices.
                FormField::new(ATTR_OPTIONS, BasicType::Json)
                    .label("Options (a fixed list of allowed values)"),
                FormField::new(ATTR_REGEX, BasicType::Text)
                    .label("Pattern (a regular expression the value must match)"),
            ],
        }
    }
}

impl Default for StringType {
    fn default() -> StringType {
        StringType::new()
    }
}

impl RichType for StringType {
    fn name(&self) -> &str {
        StringType::NAME
    }

    fn attributes(&self) -> &[FormField] {
        &self.attributes
    }

    fn validate(&self, value: &Value, attrs: &Attrs) -> Result<()> {
        let Some(s) = as_text(value)? else {
            return Ok(()); // null — a field-level (nullability) concern, not ours.
        };

        if let Some(max) = attrs
            .get(ATTR_MAX_LENGTH)
            .and_then(serde_json::Value::as_i64)
        {
            let len = s.chars().count() as i64;
            if len > max {
                return Err(Error::invalid(format!(
                    "must be at most {max} characters, got {len}"
                )));
            }
        }

        // `options`, when a non-empty array, restricts the value to that set.
        if let Some(options) = attrs
            .get(ATTR_OPTIONS)
            .and_then(serde_json::Value::as_array)
        {
            if !options.is_empty() && !options.iter().any(|o| o.as_str() == Some(s)) {
                let allowed = options
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(Error::invalid(format!(
                    "must be one of {allowed}, got {s:?}"
                )));
            }
        }

        // `regex`, when set, must match the whole value.
        if let Some(pattern) = attrs.get(ATTR_REGEX).and_then(serde_json::Value::as_str) {
            if !pattern.is_empty() && !matches_pattern(pattern, s)? {
                return Err(Error::invalid(format!(
                    "must match the pattern `{pattern}`, got {s:?}"
                )));
            }
        }
        Ok(())
    }

    fn sql_types(&self) -> &[&str] {
        const TYPES: &[&str] = &["text"];
        TYPES
    }
}

/// A 64-bit integer with an optional minimum and maximum (design §2.2).
pub struct IntegerType {
    attributes: Vec<FormField>,
}

impl IntegerType {
    /// The registry name.
    pub const NAME: &'static str = "integer";

    /// An `Integer` type with its attribute spec built.
    pub fn new() -> IntegerType {
        IntegerType {
            attributes: vec![
                FormField::new(ATTR_MIN, BasicType::Int).label("Minimum"),
                FormField::new(ATTR_MAX, BasicType::Int).label("Maximum"),
            ],
        }
    }
}

impl Default for IntegerType {
    fn default() -> IntegerType {
        IntegerType::new()
    }
}

impl RichType for IntegerType {
    fn name(&self) -> &str {
        IntegerType::NAME
    }

    fn attributes(&self) -> &[FormField] {
        &self.attributes
    }

    fn validate(&self, value: &Value, attrs: &Attrs) -> Result<()> {
        let n = match value {
            Value::Null => return Ok(()),
            Value::Int(n) => *n,
            other => {
                return Err(Error::invalid(format!(
                    "must be an integer, got a value of kind `{}`",
                    other.kind()
                )));
            }
        };

        if let Some(min) = attrs.get(ATTR_MIN).and_then(serde_json::Value::as_i64) {
            if n < min {
                return Err(Error::invalid(format!("must be at least {min}, got {n}")));
            }
        }
        if let Some(max) = attrs.get(ATTR_MAX).and_then(serde_json::Value::as_i64) {
            if n > max {
                return Err(Error::invalid(format!("must be at most {max}, got {n}")));
            }
        }
        Ok(())
    }

    fn sql_types(&self) -> &[&str] {
        const TYPES: &[&str] = &["int8"];
        TYPES
    }
}

/// A value expected to be text: `Some(text)`, `None` for null, or an error for a
/// value of the wrong family.
fn as_text(value: &Value) -> Result<Option<&str>> {
    match value {
        Value::Null => Ok(None),
        Value::Text(s) => Ok(Some(s)),
        other => Err(Error::invalid(format!(
            "must be text, got a value of kind `{}`",
            other.kind()
        ))),
    }
}

/// Whether `value` matches `pattern` in full.
///
/// The pattern is anchored to the whole string (`^(?:…)$`) because a field
/// constraint means "the value is one of these", not "the value contains one" —
/// an unanchored `\d+` would pass `"abc123"`, which is a surprising thing for a
/// validation rule to allow.
///
/// A pattern that will not compile is the admin's mistake, surfaced as an error
/// naming it. Ideally that is caught when the field is saved (§3.3); until then
/// it is caught here, at the write, rather than silently passing everything.
/// Recompiling per call is acceptable at this altitude — caching compiled
/// patterns is an optimisation for when a hot path needs it.
fn matches_pattern(pattern: &str, value: &str) -> Result<bool> {
    let anchored = format!("^(?:{pattern})$");
    let re = regex_lite::Regex::new(&anchored).map_err(|e| {
        Error::invalid(format!(
            "`{pattern}` is not a valid regular expression: {e}"
        ))
    })?;
    Ok(re.is_match(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RichTypeRef, TypeRef};
    use serde_json::json;

    /// Build an `Attrs` bag from `(key, json)` pairs.
    fn attrs(pairs: &[(&str, serde_json::Value)]) -> Attrs {
        let mut a = Attrs::new();
        for (k, v) in pairs {
            a.insert((*k).to_owned(), v.clone());
        }
        a
    }

    #[test]
    fn string_validates_value_family_max_length_and_options() {
        let t = StringType::new();
        let none = Attrs::new();

        // Family: text passes, null passes, other kinds do not.
        assert!(t.validate(&Value::Text("hi".into()), &none).is_ok());
        assert!(t.validate(&Value::Null, &none).is_ok());
        assert!(t.validate(&Value::Int(3), &none).is_err());

        // max_length drives validation; the message names the limit and the
        // actual length (counted in characters, not bytes).
        let capped = attrs(&[(ATTR_MAX_LENGTH, json!(3))]);
        assert!(t.validate(&Value::Text("abc".into()), &capped).is_ok());
        let err = t
            .validate(&Value::Text("abcd".into()), &capped)
            .unwrap_err()
            .to_string();
        assert!(err.contains('3') && err.contains('4'), "{err}");
        // A multi-byte character counts as one.
        assert!(t.validate(&Value::Text("épé".into()), &capped).is_ok());

        // options restricts to the set; an empty list restricts nothing.
        let choice = attrs(&[(ATTR_OPTIONS, json!(["red", "green"]))]);
        assert!(t.validate(&Value::Text("red".into()), &choice).is_ok());
        let err = t
            .validate(&Value::Text("blue".into()), &choice)
            .unwrap_err()
            .to_string();
        assert!(err.contains("red") && err.contains("blue"), "{err}");
        let empty = attrs(&[(ATTR_OPTIONS, json!([]))]);
        assert!(t.validate(&Value::Text("anything".into()), &empty).is_ok());
    }

    #[test]
    fn string_regex_matches_in_full_and_reports_bad_patterns() {
        let t = StringType::new();

        // A postcode-ish pattern: three digits. The anchor makes it a full-match
        // rule, so a value that merely *contains* three digits is rejected.
        let digits = attrs(&[(ATTR_REGEX, json!(r"\d{3}"))]);
        assert!(t.validate(&Value::Text("123".into()), &digits).is_ok());
        let err = t
            .validate(&Value::Text("abc123".into()), &digits)
            .unwrap_err()
            .to_string();
        assert!(err.contains(r"\d{3}"), "{err}");
        assert!(t.validate(&Value::Text("12".into()), &digits).is_err());

        // An empty pattern restricts nothing.
        let empty = attrs(&[(ATTR_REGEX, json!(""))]);
        assert!(t.validate(&Value::Text("whatever".into()), &empty).is_ok());

        // A user-anchored pattern still works (double-anchoring is harmless).
        let anchored = attrs(&[(ATTR_REGEX, json!("^a.*z$"))]);
        assert!(t.validate(&Value::Text("abcz".into()), &anchored).is_ok());
        assert!(t.validate(&Value::Text("abc".into()), &anchored).is_err());

        // An invalid pattern is the admin's mistake, surfaced by name.
        let broken = attrs(&[(ATTR_REGEX, json!("("))]);
        let err = t
            .validate(&Value::Text("x".into()), &broken)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a valid regular expression"), "{err}");
    }

    #[test]
    fn integer_validates_value_family_and_range() {
        let t = IntegerType::new();
        let none = Attrs::new();

        assert!(t.validate(&Value::Int(5), &none).is_ok());
        assert!(t.validate(&Value::Null, &none).is_ok());
        assert!(t.validate(&Value::Text("5".into()), &none).is_err());

        let ranged = attrs(&[(ATTR_MIN, json!(1)), (ATTR_MAX, json!(10))]);
        assert!(t.validate(&Value::Int(1), &ranged).is_ok());
        assert!(t.validate(&Value::Int(10), &ranged).is_ok());
        let low = t.validate(&Value::Int(0), &ranged).unwrap_err().to_string();
        assert!(low.contains('1'), "{low}");
        let high = t
            .validate(&Value::Int(11), &ranged)
            .unwrap_err()
            .to_string();
        assert!(high.contains("10"), "{high}");

        // min and max are independent — either alone is respected.
        let floor = attrs(&[(ATTR_MIN, json!(0))]);
        assert!(t.validate(&Value::Int(-1), &floor).is_err());
        assert!(t.validate(&Value::Int(1_000_000), &floor).is_ok());
    }

    #[test]
    fn each_type_round_trips_its_sql_type_through_a_ref() {
        // `sql_type()` on a `TypeRef::Rich` must be the DDL type the field's
        // column actually has, so the two agree at §3.2's merge.
        for (name, expected) in [
            (StringType::NAME, "text"),
            (IntegerType::NAME, "int8"),
            (COLLAB_DOC, "bytea"),
        ] {
            let r = RichTypeRef::resolve(name).expect("registered");
            assert_eq!(r.sql_type(), expected, "{name}");
            assert_eq!(TypeRef::rich(r).sql_type(), expected, "{name}");
        }
    }

    #[test]
    fn a_collab_doc_holds_bytes_and_says_so() {
        let t = CollabDocType;
        let none = Attrs::new();
        assert!(t.validate(&Value::Bytes(vec![1, 2]), &none).is_ok());
        assert!(t.validate(&Value::Null, &none).is_ok());
        let err = t
            .validate(&Value::Text("hello".into()), &none)
            .unwrap_err()
            .to_string();
        assert!(err.contains("stored as bytes"), "{err}");
        let r = TypeRef::rich(RichTypeRef::resolve(COLLAB_DOC).unwrap());
        assert!(is_collab_doc(&r));
        assert!(!is_collab_doc(&TypeRef::Basic(BasicType::Bytes)));
    }
}
