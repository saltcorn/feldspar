//! SQL identifiers as a type (security review 2026-11 §7, D3–D5;
//! TODO-type-hardening H6.2).
//!
//! An [`Ident`] is a table, column, constraint or index name that has been
//! checked once, where it entered: [`Ident::new`] for a name this program is
//! about to **create** (the schema editor, a module's tables, CSV and geo
//! import), [`Ident::existing`] for one it **found** (introspection, the
//! catalog's overlay rows, a backup). The second refuses only what no database
//! can hold, so every application keeps loading whatever names its database
//! already has; the first is the rule new names have always been held to.
//!
//! An `Ident` has no `Display`: the way into SQL text is
//! [`SqlDialect::quote`](crate::SqlDialect::quote), which gives a
//! [`QuotedIdent`]. It serializes as its text (and deserializes through
//! [`Ident::existing`]), so a stored definition carrying one reads as before.
//!
//! [`Ident::derived`] names what Postgres names after a table (`<table>_pkey`)
//! by Postgres' own rule, which truncates the *table name* so that the whole
//! fits in 63 bytes — not the concatenation (D3).

use std::borrow::Borrow;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

/// The longest identifier Postgres keeps: `NAMEDATALEN - 1` bytes. A longer
/// one is silently truncated to this, at a character boundary.
pub const MAX_IDENT_BYTES: usize = 63;

/// Why a name is not an identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentError {
    /// The empty name.
    Empty,
    /// Longer than [`MAX_IDENT_BYTES`] (only refused for a new name).
    TooLong {
        /// The name.
        name: String,
    },
    /// A NUL byte, which no database stores in a name.
    Nul,
    /// A character outside `[A-Za-z0-9_]`, or a first character that is not a
    /// letter or `_` (only refused for a new name).
    Charset {
        /// The name.
        name: String,
    },
}

impl IdentError {
    /// The refusal as a sentence about the `what` (`table`, `field`, …) being
    /// named.
    pub fn sentence(&self, what: &str) -> String {
        match self {
            IdentError::Empty => format!("a {what} needs a name"),
            IdentError::TooLong { name } => format!(
                "{what} name `{name}` is longer than the {MAX_IDENT_BYTES} characters a database \
                 identifier allows"
            ),
            IdentError::Nul => format!("a {what} name cannot contain a NUL character"),
            IdentError::Charset { name } => format!(
                "{what} name `{name}` must start with a letter or underscore and contain only \
                 letters, digits and underscores"
            ),
        }
    }
}

impl fmt::Display for IdentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.sentence("database object"))
    }
}

impl std::error::Error for IdentError {}

impl From<IdentError> for sc_error::Error {
    fn from(e: IdentError) -> sc_error::Error {
        sc_error::Error::invalid(e.to_string())
    }
}

/// A checked SQL identifier (see the module docs).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Ident(Box<str>);

impl Ident {
    /// A name this program is about to create: non-empty, at most
    /// [`MAX_IDENT_BYTES`] bytes, a letter or `_` and then letters, digits and
    /// `_` — the rule the schema editor has always applied
    /// (`check_identifier`), so nothing that was accepted is refused.
    pub fn new(name: &str) -> Result<Ident, IdentError> {
        if name.is_empty() {
            return Err(IdentError::Empty);
        }
        if name.len() > MAX_IDENT_BYTES {
            return Err(IdentError::TooLong {
                name: name.to_owned(),
            });
        }
        let mut chars = name.chars();
        let first = chars.next().unwrap_or_default();
        if !(first.is_ascii_alphabetic() || first == '_')
            || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(IdentError::Charset {
                name: name.to_owned(),
            });
        }
        Ok(Ident(name.into()))
    }

    /// A name a database already has, or that stored data carries: anything
    /// but a NUL byte (which no database stores in a name). Quotes, spaces,
    /// `$`, non-ASCII and — on SQLite — any length are kept, because a
    /// database somebody else built has them and quoting makes them safe.
    pub fn existing(name: &str) -> Result<Ident, IdentError> {
        if name.contains('\0') {
            return Err(IdentError::Nul);
        }
        Ok(Ident(name.into()))
    }

    /// The text of the name. For a check or a message — SQL goes through
    /// [`SqlDialect::quote`](crate::SqlDialect::quote).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name Postgres gives what it names after this one with `label`:
    /// `<name>_<label>`, with the **name** cut short, at a character boundary,
    /// so that the whole is at most [`MAX_IDENT_BYTES`] bytes —
    /// `makeObjectName(name, NULL, label)` in Postgres' `indexcmds.c`. A
    /// table's primary key is `derived("pkey")`.
    ///
    /// This is the name Postgres tries first. Where something of that name
    /// already exists in the schema, Postgres appends a number to the label
    /// (`<name>_pkey1`), which only the database knows: so DDL finds a
    /// table's primary key by what it is (`contype = 'p'`), not by this name.
    #[must_use]
    pub fn derived(&self, label: &str) -> Ident {
        let available = MAX_IDENT_BYTES.saturating_sub(label.len().saturating_add(1));
        let stem = clip(&self.0, available);
        Ident(format!("{stem}_{label}").into())
    }

    /// The name Postgres actually keeps for this one: at most
    /// [`MAX_IDENT_BYTES`] bytes, clipped at a character boundary
    /// (`truncate_identifier`). The name itself when it already fits.
    #[must_use]
    pub fn as_postgres_keeps_it(&self) -> Ident {
        Ident(clip(&self.0, MAX_IDENT_BYTES).into())
    }
}

/// The longest prefix of `s` of at most `max` bytes that ends on a character
/// boundary (Postgres' `pg_mbcliplen` for a UTF-8 database).
fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let end = s
        .char_indices()
        .map(|(i, c)| i.saturating_add(c.len_utf8()))
        .take_while(|end| *end <= max)
        .last()
        .unwrap_or(0);
    s.get(..end).unwrap_or("")
}

impl fmt::Debug for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl Borrow<str> for Ident {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Ident {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for Ident {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for Ident {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl<'de> Deserialize<'de> for Ident {
    /// Through [`Ident::existing`], so a stored name loads as it was stored.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Ident, D::Error> {
        let text = String::deserialize(deserializer)?;
        Ident::existing(&text).map_err(serde::de::Error::custom)
    }
}

/// An identifier quoted for one dialect: SQL text, so it has `Display`. Made
/// only by [`SqlDialect::quote`](crate::SqlDialect::quote).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QuotedIdent(String);

impl QuotedIdent {
    /// Wrap a dialect's quoting of a name. Crate-private: the one way to make
    /// one is to quote an [`Ident`].
    pub(crate) fn quoted(sql: String) -> QuotedIdent {
        QuotedIdent(sql)
    }

    /// The quoted text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for QuotedIdent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_name_is_held_to_the_schema_editors_rule() {
        for good in ["books", "_x", "a1_b2", "a".repeat(63).as_str()] {
            assert!(Ident::new(good).is_ok(), "{good}");
        }
        assert_eq!(Ident::new(""), Err(IdentError::Empty));
        assert!(matches!(
            Ident::new(&"a".repeat(64)),
            Err(IdentError::TooLong { .. })
        ));
        for bad in ["1a", "a b", "a\"b", "a$b", "é", "a-b", "a\0b"] {
            assert!(
                matches!(Ident::new(bad), Err(IdentError::Charset { .. })),
                "{bad:?}"
            );
        }
        assert_eq!(
            Ident::new("a b").unwrap_err().sentence("table"),
            "table name `a b` must start with a letter or underscore and contain only \
             letters, digits and underscores"
        );
    }

    #[test]
    fn an_existing_name_keeps_whatever_a_database_can_hold() {
        let long = "x".repeat(200);
        for name in ["Order Lines", "a\"b", "$sc$", "naïve", "", long.as_str()] {
            assert_eq!(Ident::existing(name).unwrap().as_str(), name);
        }
        assert_eq!(Ident::existing("a\0b"), Err(IdentError::Nul));
    }

    #[test]
    fn it_serializes_as_its_text_and_loads_through_existing() {
        let ident = Ident::existing("Order Lines").unwrap();
        assert_eq!(serde_json::to_string(&ident).unwrap(), "\"Order Lines\"");
        let back: Ident = serde_json::from_str("\"Order Lines\"").unwrap();
        assert_eq!(back, ident);
        assert!(serde_json::from_str::<Ident>("\"a\\u0000b\"").is_err());
    }

    #[test]
    fn a_derived_name_truncates_the_stem_as_postgres_does() {
        let short = Ident::new("invoice").unwrap();
        assert_eq!(short.derived("pkey").as_str(), "invoice_pkey");
        // 58 bytes of stem leave room for `_pkey`: unchanged at 58, cut at 59+.
        let at = "t".repeat(58);
        assert_eq!(
            Ident::new(&at).unwrap().derived("pkey").as_str(),
            format!("{at}_pkey")
        );
        let over = format!("{}abcde", "t".repeat(58));
        let derived = Ident::new(&over).unwrap().derived("pkey");
        assert_eq!(derived.as_str(), format!("{}_pkey", "t".repeat(58)));
        assert_eq!(derived.as_str().len(), MAX_IDENT_BYTES);
        // A multi-byte character that would straddle the cut is dropped whole.
        let wide = Ident::existing(&format!("{}é", "t".repeat(57))).unwrap();
        assert_eq!(wide.as_str().len(), 59);
        assert_eq!(
            wide.derived("pkey").as_str(),
            format!("{}_pkey", "t".repeat(57))
        );
    }

    #[test]
    fn postgres_keeps_63_bytes_at_a_character_boundary() {
        let long = Ident::existing(&format!("{}éé", "a".repeat(62))).unwrap();
        assert_eq!(long.as_postgres_keeps_it().as_str(), "a".repeat(62));
        let fits = Ident::existing("short").unwrap();
        assert_eq!(fits.as_postgres_keeps_it(), fits);
    }
}
