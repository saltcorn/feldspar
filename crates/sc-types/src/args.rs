//! Reading the members of a JSON object handed in from outside: a tool call's
//! arguments, a request body.
//!
//! Where a body has a fixed shape, deserialize it into a struct instead. This is
//! for what is left: arguments read one at a time, whose refusals must name the
//! member and say what it should have been. Two families, because two kinds of
//! caller read refusals differently and already rely on their wording:
//!
//! - **arguments** (tools, actions): "`` `name` is required ``",
//!   "`` `name` should be a string, got 5 ``";
//! - **fields** (the admin API's bodies): "missing or non-string field `name`",
//!   "field `name` must not be empty".
//!
//! `null` counts as absent everywhere: a JSON client that sends `"name": null`
//! means it did not set it.

use sc_error::{Error, Result};
use serde_json::{Map, Value as Json};

/// The readers, on a JSON object's members.
pub trait ArgsExt {
    /// The member `key` unless it is absent or `null`.
    fn present(&self, key: &str) -> Option<&Json>;

    /// A string argument that may be absent; anything else present is refused.
    fn optional_string(&self, key: &str) -> Result<Option<String>> {
        match self.present(key) {
            None => Ok(None),
            Some(Json::String(s)) => Ok(Some(s.clone())),
            Some(other) => Err(Error::invalid(format!(
                "`{key}` should be a string, got {other}"
            ))),
        }
    }

    /// A string argument that may be absent, `""` when it is.
    fn string_or_empty(&self, key: &str) -> Result<String> {
        Ok(self.optional_string(key)?.unwrap_or_default())
    }

    /// A string argument that must be there, as it was sent (an empty string
    /// is one).
    fn string_arg(&self, key: &str) -> Result<String> {
        self.optional_string(key)?
            .ok_or_else(|| Error::invalid(format!("`{key}` is required")))
    }

    /// A string argument that must be there and not blank, trimmed.
    fn required_string(&self, key: &str) -> Result<String> {
        self.optional_string(key)?
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::invalid(format!("`{key}` is required")))
    }

    /// A boolean argument that may be absent.
    fn optional_bool(&self, key: &str) -> Result<Option<bool>> {
        match self.present(key) {
            None => Ok(None),
            Some(Json::Bool(b)) => Ok(Some(*b)),
            Some(other) => Err(Error::invalid(format!(
                "`{key}` should be true or false, got {other}"
            ))),
        }
    }

    /// An argument of any type that must be there.
    fn required_value(&self, key: &str) -> Result<&Json> {
        self.present(key)
            .ok_or_else(|| Error::invalid(format!("`{key}` is required")))
    }

    /// A body's string field that must be there (an empty string is one).
    fn str_field(&self, key: &str) -> Result<&str> {
        self.present(key)
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid(format!("missing or non-string field `{key}`")))
    }

    /// A body's string field that must be there and not blank, as sent.
    fn non_empty_str_field(&self, key: &str) -> Result<&str> {
        let value = self.str_field(key)?;
        if value.trim().is_empty() {
            return Err(Error::invalid(format!("field `{key}` must not be empty")));
        }
        Ok(value)
    }
}

impl ArgsExt for Map<String, Json> {
    fn present(&self, key: &str) -> Option<&Json> {
        self.get(key).filter(|v| !v.is_null())
    }
}

/// On a value, the members of an object; anything else has none.
impl ArgsExt for Json {
    fn present(&self, key: &str) -> Option<&Json> {
        self.get(key).filter(|v| !v.is_null())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(value: Json) -> Map<String, Json> {
        match value {
            Json::Object(map) => map,
            _ => Map::new(),
        }
    }

    fn refusal<T: std::fmt::Debug>(r: Result<T>) -> String {
        match r {
            Ok(v) => format!("accepted {v:?}"),
            Err(e) => e.message().to_owned(),
        }
    }

    #[test]
    fn null_is_absent_and_the_wrong_type_is_named() {
        let a = args(json!({ "s": "x", "n": null, "five": 5, "blank": "  ", "b": true }));
        assert_eq!(a.optional_string("n").ok(), Some(None));
        assert_eq!(a.optional_string("gone").ok(), Some(None));
        assert_eq!(
            refusal(a.optional_string("five")),
            "`five` should be a string, got 5"
        );
        assert_eq!(a.string_or_empty("n").ok().as_deref(), Some(""));
        assert_eq!(refusal(a.string_arg("n")), "`n` is required");
        assert_eq!(a.string_arg("blank").ok().as_deref(), Some("  "));
        assert_eq!(refusal(a.required_string("blank")), "`blank` is required");
        assert_eq!(a.required_string("s").ok().as_deref(), Some("x"));
        assert_eq!(a.optional_bool("b").ok(), Some(Some(true)));
        assert_eq!(
            refusal(a.optional_bool("s")),
            "`s` should be true or false, got \"x\""
        );
        assert_eq!(refusal(a.required_value("n")), "`n` is required");
        assert_eq!(a.required_value("five").ok(), Some(&json!(5)));
    }

    #[test]
    fn a_body_field_is_refused_in_the_admin_apis_words() {
        let body = json!({ "name": "  ", "count": 1, "title": "T" });
        assert_eq!(
            refusal(body.str_field("count")),
            "missing or non-string field `count`"
        );
        assert_eq!(
            refusal(body.str_field("gone")),
            "missing or non-string field `gone`"
        );
        assert_eq!(body.str_field("name").ok(), Some("  "));
        assert_eq!(
            refusal(body.non_empty_str_field("name")),
            "field `name` must not be empty"
        );
        assert_eq!(body.non_empty_str_field("title").ok(), Some("T"));
        // Not an object: no members.
        assert_eq!(refusal(json!([1]).required_value("x")), "`x` is required");
    }
}
