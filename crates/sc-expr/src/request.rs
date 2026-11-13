//! Presented values: `_request` (TODO-type-hardening H7.5, TECHNICAL_DESIGN
//! §7.3a).
//!
//! A share link, an unsubscribe link or an invitation must be readable by
//! whoever holds its secret and listable by nobody. An ownership formula says
//! so by comparing a column with a value the caller presents:
//!
//! ```js
//! _read && token === _request.token
//! ```
//!
//! Because the equality is decided per row, a caller only reaches the row
//! whose secret they already hold. Two things keep that true, and both live
//! here:
//!
//! - [`RequestValues`]: what a caller presented, by key. Keys are
//!   `[a-z][a-z0-9_]{0,31}`, at most [`MAX_REQUEST_KEYS`]; a value that is
//!   empty is **absent**, and an absent value equals nothing, not even a null
//!   column (`undefined` on the evaluator, SQL `NULL` in a translation, and a
//!   comparison that is never true).
//! - [`check_request_uses`]: the shape a formula may use `_request` in,
//!   checked on save. `_request` is caller-controlled and sits next to the
//!   trusted `user`, and `owner === _request.user_id` would read plausibly to
//!   an agent and give away every row. So `_request.<key>` may only be one
//!   side of `===` whose other side is a unique column (of this table, or
//!   reached by a Ⱶ path) that is neither the primary key nor a number the
//!   database counts up.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use sc_error::{Error, Result};
use sc_query::Value;

use crate::analyze::{Ambient, JOIN};
use crate::ast::{Ast, BinaryOp, MemberProp};
use crate::formula::Formula;
use crate::shape::{FieldKind, FieldShape, SchemaShape};

/// How many values one request may present.
pub const MAX_REQUEST_KEYS: usize = 8;

/// The longest key, in bytes.
pub const MAX_REQUEST_KEY_BYTES: usize = 32;

/// The prefix of the GUC a presented value is set in for RLS:
/// `sc.request.<key>`.
pub const REQUEST_GUC_PREFIX: &str = "sc.request.";

/// Whether `key` is a presented value's key: `[a-z][a-z0-9_]{0,31}`.
pub fn is_request_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && key.len() <= MAX_REQUEST_KEY_BYTES
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// The GUC `key`'s presented value is set in: `sc.request.<key>`. The key is
/// one [`is_request_key`] accepts, so the name needs no quoting.
pub fn request_guc(key: &str) -> String {
    format!("{REQUEST_GUC_PREFIX}{key}")
}

/// The values a caller presented with one request, by key.
///
/// Built only by [`RequestValues::new`], which checks the keys and drops the
/// empty values. `Debug` names the keys and not the values: a presented value
/// is a secret, and a debug line is a log line.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct RequestValues(BTreeMap<String, String>);

impl RequestValues {
    /// The presented values, from `(key, value)` pairs. An empty value is
    /// absent. A key outside `[a-z][a-z0-9_]{0,31}`, the same key twice, and
    /// more than [`MAX_REQUEST_KEYS`] values are refused, naming the problem.
    pub fn new<K, V>(pairs: impl IntoIterator<Item = (K, V)>) -> Result<RequestValues>
    where
        K: AsRef<str>,
        V: Into<String>,
    {
        let mut values = BTreeMap::new();
        for (key, value) in pairs {
            let key = key.as_ref();
            if !is_request_key(key) {
                return Err(Error::invalid(format!(
                    "`{}` is not a presented value's name: it is a lowercase letter, then up \
                     to 31 lowercase letters, digits or `_`",
                    key.chars().take(40).collect::<String>()
                )));
            }
            let value = value.into();
            if values.contains_key(key) {
                return Err(Error::invalid(format!("`{key}` is presented twice")));
            }
            if value.is_empty() {
                continue;
            }
            values.insert(key.to_owned(), value);
        }
        if values.len() > MAX_REQUEST_KEYS {
            return Err(Error::invalid(format!(
                "a request presents at most {MAX_REQUEST_KEYS} values, and this one has {}",
                values.len()
            )));
        }
        Ok(RequestValues(values))
    }

    /// The value presented under `key`, if one was.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Whether nothing was presented.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The keys presented.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// The `_request` object a formula sees: each value as text.
    pub fn to_ambient(&self) -> BTreeMap<String, Value> {
        self.0
            .iter()
            .map(|(k, v)| (k.clone(), Value::Text(v.clone())))
            .collect()
    }
}

impl fmt::Debug for RequestValues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

/// What [`check_request_uses`] found in an accepted formula.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestUses {
    /// The keys the formula reads, which RLS sets a GUC for.
    pub keys: BTreeSet<String>,
    /// Advice that does not refuse the formula: a column compared with a
    /// presented value that has no random default.
    pub warnings: Vec<String>,
}

/// Check every use of `_request` in an ownership formula on `table` (see the
/// module docs): each must be `column === _request.key`, the column unique,
/// not the primary key and not a counted number. A use outside that shape is
/// refused with the reason; a column without a random default is a warning.
///
/// A formula that does not name `_request` (or on a table where a field of
/// that name shadows it) passes with nothing found.
pub fn check_request_uses(
    formula: &Formula,
    shape: &SchemaShape,
    table: &str,
) -> Result<RequestUses> {
    let mut uses = RequestUses::default();
    if !shape.request_on(table) {
        return Ok(uses);
    }
    walk(formula.ast(), shape, table, &mut Vec::new(), &mut uses)?;
    if uses.keys.len() > MAX_REQUEST_KEYS {
        return Err(refused(
            table,
            &format!(
                "it reads {} presented values, and a request presents at most \
                 {MAX_REQUEST_KEYS}",
                uses.keys.len()
            ),
        ));
    }
    Ok(uses)
}

fn refused(table: &str, why: &str) -> Error {
    Error::invalid(format!(
        "the ownership formula of `{table}` uses `_request` in a way that is not allowed: {why}. \
         A presented value may only be compared with a unique column, as \
         `token === _request.token`"
    ))
}

/// The key of `_request.<key>`, when `ast` is that (unshadowed).
fn request_key<'a>(ast: &'a Ast, locals: &[String]) -> Option<&'a str> {
    match ast {
        Ast::Member {
            obj,
            prop: MemberProp::Static(key),
            ..
        } if is_request_object(obj, locals) => Some(key),
        _ => None,
    }
}

fn is_request_object(ast: &Ast, locals: &[String]) -> bool {
    matches!(ast, Ast::Ident(name)
        if name == Ambient::Request.as_str() && !locals.iter().any(|l| l == name))
}

fn walk(
    ast: &Ast,
    shape: &SchemaShape,
    table: &str,
    locals: &mut Vec<String>,
    uses: &mut RequestUses,
) -> Result<()> {
    if let Ast::Binary { op, l, r } = ast
        && !matches!(op, BinaryOp::And | BinaryOp::Or | BinaryOp::Nullish)
    {
        let (key, other) = match (request_key(l, locals), request_key(r, locals)) {
            (Some(k), None) => (Some(k), &**r),
            (None, Some(k)) => (Some(k), &**l),
            (Some(_), Some(_)) => {
                return Err(refused(
                    table,
                    "two presented values are compared with each other, which says nothing \
                     about the row",
                ));
            }
            (None, None) => (None, &**l),
        };
        if let Some(key) = key {
            match op {
                BinaryOp::StrictEq => {}
                BinaryOp::StrictNotEq | BinaryOp::NotEq => {
                    return Err(refused(
                        table,
                        &format!(
                            "`_request.{key}` is compared with `!==`, which would grant every \
                             row whose value differs from what the caller sent"
                        ),
                    ));
                }
                BinaryOp::Eq => {
                    return Err(refused(
                        table,
                        &format!("compare `_request.{key}` with `===`, not `==`"),
                    ));
                }
                _ => {
                    return Err(refused(
                        table,
                        &format!("`_request.{key}` is used in arithmetic or an ordering"),
                    ));
                }
            }
            if !crate::request::is_request_key(key) {
                return Err(refused(
                    table,
                    &format!(
                        "`{key}` is not a presented value's name (a lowercase letter, then up \
                         to 31 lowercase letters, digits or `_`)"
                    ),
                ));
            }
            check_column(other, key, shape, table, locals, uses)?;
            uses.keys.insert(key.to_owned());
            return Ok(());
        }
    }
    match ast {
        // `_request.key` anywhere but `column === _request.key`.
        _ if request_key(ast, locals).is_some() => {
            let key = request_key(ast, locals).unwrap_or_default();
            Err(refused(
                table,
                &format!(
                    "`_request.{key}` is used other than as one side of `column === \
                     _request.{key}` (as a condition, a value, or an argument)"
                ),
            ))
        }
        Ast::Member { obj, .. } if is_request_object(obj, locals) => Err(refused(
            table,
            "`_request` is indexed with a computed key; name the key, as `_request.token`",
        )),
        _ if is_request_object(ast, locals) => Err(refused(
            table,
            "`_request` itself is used; compare one of its keys with a column",
        )),
        Ast::Arrow { params, body } => {
            let depth = locals.len();
            locals.extend(params.iter().cloned());
            let result = walk(body, shape, table, locals, uses);
            locals.truncate(depth);
            result
        }
        other => {
            for child in crate::analyze::child_nodes(other) {
                walk(child, shape, table, locals, uses)?;
            }
            Ok(())
        }
    }
}

/// Check the column a presented value is compared with.
fn check_column(
    other: &Ast,
    key: &str,
    shape: &SchemaShape,
    table: &str,
    locals: &[String],
    uses: &mut RequestUses,
) -> Result<()> {
    let not_a_column = || {
        refused(
            table,
            &format!(
                "`_request.{key}` is compared with something that is not a column of `{table}` \
                 or one reached by a Ⱶ path (`user`, `old`, a literal, a child row or an \
                 expression); `_request` is chosen by the caller, so only a column decides \
                 which row it reaches"
            ),
        )
    };
    let Ast::Ident(name) = other else {
        return Err(not_a_column());
    };
    if locals.iter().any(|l| l == name) || Ambient::from_ident(name).is_some() {
        return Err(not_a_column());
    }
    let Some((column_table, column, field)) = resolve_column(shape, table, name) else {
        return Err(not_a_column());
    };
    let named = format!("`{column_table}.{column}`");
    if field.primary_key {
        return Err(refused(
            table,
            &format!(
                "{named} is the primary key, which the table's own routes and clients already \
                 name rows by"
            ),
        ));
    }
    if field.kind == FieldKind::Integer || field.identity {
        return Err(refused(
            table,
            &format!("{named} is a whole number, which a caller can count through"),
        ));
    }
    if !field.unique {
        return Err(refused(
            table,
            &format!(
                "{named} is not unique, so one presented value could reach many rows; make it \
                 unique"
            ),
        ));
    }
    if !field.random_default {
        uses.warnings.push(format!(
            "{named} is compared with `_request.{key}` but has no random default, so its \
             values may be ones people chose and can guess. Give it the default \
             `gen_random_uuid()`"
        ));
    }
    Ok(())
}

/// The table, column and shape of a column `name` reads on `table`: one of its
/// own, or the last field of a Ⱶ path.
fn resolve_column<'a>(
    shape: &'a SchemaShape,
    table: &str,
    name: &'a str,
) -> Option<(String, &'a str, &'a FieldShape)> {
    let mut current = table.to_owned();
    let segments: Vec<&str> = name.split(JOIN).collect();
    let (last, links) = segments.split_last()?;
    for link in links {
        let key = shape
            .tables
            .get(&current)?
            .fields
            .get(*link)?
            .key
            .as_ref()?;
        current = key.target_table.clone();
    }
    let field = shape.tables.get(&current)?.fields.get(*last)?;
    Some((current, last, field))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shape::TableShape;

    fn shape() -> SchemaShape {
        SchemaShape::new()
            .table(
                "shares",
                TableShape::new()
                    .typed_field("id", FieldKind::Integer)
                    .with_field("id", |f| {
                        f.primary_key = true;
                        f.identity = true;
                    })
                    .typed_field("token", FieldKind::Text)
                    .with_field("token", |f| {
                        f.unique = true;
                        f.random_default = true;
                    })
                    .typed_field("chosen", FieldKind::Text)
                    .with_field("chosen", |f| f.unique = true)
                    .typed_field("owner", FieldKind::Text)
                    .typed_field("serial", FieldKind::Integer)
                    .with_field("serial", |f| f.unique = true)
                    .key_field("invite", "invites", "id"),
            )
            .table(
                "invites",
                TableShape::new()
                    .typed_field("id", FieldKind::Integer)
                    .typed_field("code", FieldKind::Text)
                    .with_field("code", |f| {
                        f.unique = true;
                        f.random_default = true;
                    }),
            )
            .user_fields(["id", "email"])
            .with_request()
            .with_old_row()
    }

    fn check(src: &str) -> Result<RequestUses> {
        let formula = Formula::parse(src)?;
        formula.validate(&shape(), "shares")?;
        check_request_uses(&formula, &shape(), "shares")
    }

    #[test]
    fn presented_values_are_named_bounded_and_empty_is_absent() {
        let values = RequestValues::new([("token", "abc"), ("invite", "")]).unwrap();
        assert_eq!(values.get("token"), Some("abc"));
        assert_eq!(values.get("invite"), None, "empty is absent");
        assert_eq!(format!("{values:?}"), "{\"token\"}", "no values in Debug");
        for bad in ["Token", "1x", "_x", "x-y", &"x".repeat(33)] {
            assert!(RequestValues::new([(bad, "v")]).is_err(), "{bad}");
        }
        assert!(RequestValues::new([("k", "a"), ("k", "b")]).is_err());
        let nine: Vec<(String, &str)> = (0..9).map(|i| (format!("k{i}"), "v")).collect();
        assert!(RequestValues::new(nine).is_err());
        let eight: Vec<(String, &str)> = (0..8).map(|i| (format!("k{i}"), "v")).collect();
        assert!(RequestValues::new(eight).is_ok());
        assert_eq!(request_guc("token"), "sc.request.token");
    }

    #[test]
    fn only_a_unique_column_equal_to_a_presented_value_is_accepted() {
        let ok = check("_read && token === _request.token").unwrap();
        assert_eq!(ok.keys, BTreeSet::from(["token".to_owned()]));
        assert!(ok.warnings.is_empty(), "{:?}", ok.warnings);
        let ok = check("_insert && inviteⱵcode === _request.invite").unwrap();
        assert_eq!(ok.keys, BTreeSet::from(["invite".to_owned()]));
        let ok = check("_request.token === token || owner === user.email").unwrap();
        assert_eq!(ok.keys.len(), 1);
        // No random default: accepted, with a warning naming the column.
        let warned = check("chosen === _request.code").unwrap();
        assert!(warned.warnings[0].contains("`shares.chosen`"), "{warned:?}");
        // Without a `_request`, nothing is found.
        assert_eq!(
            check("owner === user.email").unwrap(),
            RequestUses::default()
        );
    }

    #[test]
    fn every_other_use_is_refused_with_its_reason() {
        for (src, why) in [
            ("owner === _request.user_id", "not unique"),
            ("id === _request.id", "primary key"),
            ("serial === _request.n", "whole number"),
            ("token !== _request.token", "`!==`"),
            ("token == _request.token", "`==`"),
            ("user.email === _request.email", "not a column"),
            ("old.token === _request.token", "not a column"),
            ("\"x\" === _request.token", "not a column"),
            ("_request.a === _request.b", "each other"),
            ("_request.token && owner === user.email", "other than"),
            ("token > _request.token", "arithmetic"),
            ("_request === null", "itself"),
            ("_request[\"token\"] === token", "computed"),
        ] {
            let err = check(src).expect_err(src).to_string();
            assert!(err.contains(why), "{src}: {err}");
        }
    }
}
