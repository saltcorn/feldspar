//! [`Secret<T>`]: a password, client secret, private key or API key that does
//! not leak by accident (code-quality review §4.6, TODO-code-quality Q4.4;
//! security review #17).
//!
//! A credential held in a plain `String` field is printed by every `{:?}` of
//! the struct around it — a log line, a test failure, an error built with
//! `format!("{settings:?}")` — and written by every `Serialize` of it, which
//! is how a settings echo or a status endpoint hands it to a caller. A
//! `Secret`:
//!
//! - **redacts `Debug`**: `Secret([redacted])`, whatever it holds;
//! - **has no `Serialize`**: a struct deriving `Serialize` around one does not
//!   compile until the field says how it is written. Storage and backups name
//!   [`expose`] (or [`expose_option`]) in `#[serde(serialize_with = …)]`, which
//!   writes the value exactly as the plain field did — so a stored setting or a
//!   backup is byte-for-byte what it was — and is greppable;
//! - **deserialises as what it wraps**: loading from configuration or the
//!   database is where one is made, and the stored text is unchanged;
//! - is read with an explicit [`expose`](Secret::expose) at the one place the
//!   credential is used (the SMTP login, the TLS key, the OAuth token call).
//!
//! An in-repo type rather than the `secrecy` crate: this is all of it, and
//! `secrecy`'s `SerializableSecret` opt-in and zeroising add a dependency for
//! nothing the server uses — the process memory is not the threat here, the
//! log line is.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A value that is not printed and not serialised unless a caller says so.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Wrap `value`.
    pub const fn new(value: T) -> Secret<T> {
        Secret(value)
    }

    /// The value, for the one place it is used.
    pub fn expose(&self) -> &T {
        &self.0
    }

    /// The value, mutably (a form that edits it in place).
    pub fn expose_mut(&mut self) -> &mut T {
        &mut self.0
    }

    /// The value, unwrapped.
    pub fn into_exposed(self) -> T {
        self.0
    }
}

impl Secret<String> {
    /// The text, for the one place it is used — what `Option::map` takes
    /// (`password.as_ref().map(Secret::expose_str)`).
    pub fn expose_str(&self) -> &str {
        &self.0
    }

    /// Whether nothing is set. Answering this exposes nothing, and it is what
    /// a form showing "set" / "not set" needs.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T> From<T> for Secret<T> {
    fn from(value: T) -> Secret<T> {
        Secret(value)
    }
}

impl From<&str> for Secret<String> {
    fn from(value: &str) -> Secret<String> {
        Secret(value.to_owned())
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Secret<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Secret<T>, D::Error> {
        T::deserialize(deserializer).map(Secret)
    }
}

/// Write `secret` as the value it holds — for storage and backups only:
/// `#[serde(serialize_with = "sc_error::secret::expose")]`.
pub fn expose<T: Serialize, S: Serializer>(
    secret: &Secret<T>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    secret.0.serialize(serializer)
}

/// [`expose`] for an optional secret.
pub fn expose_option<T: Serialize, S: Serializer>(
    secret: &Option<Secret<T>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    secret.as_ref().map(|s| &s.0).serialize(serializer)
}

/// For `#[serde(skip_serializing_if = "sc_error::secret::is_empty")]`.
pub fn is_empty(secret: &Secret<String>) -> bool {
    secret.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Settings {
        host: String,
        #[serde(serialize_with = "expose")]
        password: Secret<String>,
        #[serde(default, serialize_with = "expose_option")]
        api_key: Option<Secret<String>>,
    }

    #[test]
    fn debug_never_shows_the_value() {
        let settings = Settings {
            host: "smtp.example.com".into(),
            password: Secret::from("hunter2"),
            api_key: Some("sk-123".into()),
        };
        let shown = format!("{settings:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(!shown.contains("sk-123"), "{shown}");
        assert!(shown.contains("smtp.example.com"), "{shown}");
        assert!(shown.contains("[redacted]"), "{shown}");
    }

    /// Stored text is the same with and without the wrapper, both ways.
    #[test]
    fn storage_is_byte_for_byte_what_the_plain_field_wrote() {
        #[derive(Serialize)]
        struct Plain {
            host: String,
            password: String,
            api_key: Option<String>,
        }
        let plain = serde_json::to_string(&Plain {
            host: "h".into(),
            password: "p".into(),
            api_key: None,
        })
        .unwrap();
        let wrapped = Settings {
            host: "h".into(),
            password: "p".into(),
            api_key: None,
        };
        assert_eq!(serde_json::to_string(&wrapped).unwrap(), plain);
        let back: Settings = serde_json::from_str(&plain).unwrap();
        assert_eq!(back, wrapped);
        assert_eq!(back.password.expose(), "p");
        assert!(!back.password.is_empty());
    }
}
