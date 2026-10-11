//! A subscription's filter on a `table_changes` stream, evaluated in Rust
//! (TODO.md "Live updates" §3 rule 5, L3.6).
//!
//! A page that shows board 7 subscribes with `{ board: 7 }` and wants the
//! changes to the cards **its read with that filter would return**, and no
//! others. So the filter is the REST read's own:
//!
//! - **The same vocabulary, by the same function.** The object is lowered by
//!   [`sc_api::filter::api_where`], the walk every non-URL surface (an agent's
//!   tool, a code body's `.where()`) uses, over the fields an application's
//!   REST read may filter on. An unknown field, the users table's password
//!   hash and a calculated field are refused exactly as REST refuses them, and
//!   a literal is coerced against its column (`"2026-01-01"` is a date).
//! - **The same meaning, evaluated here.** The lowered predicate is the
//!   [`Expr`] the database would run; this module evaluates it against a
//!   changed row's image, with **SQL's three-valued logic**: a comparison with a
//!   null is unknown, `NOT` of unknown is unknown, and a row matches only when
//!   the whole predicate is *true*. `eq`/`ne` never match a null; `is_null`
//!   asks about one.
//!
//! Evaluated in Rust rather than by asking the database, because the old image
//! of an update or a delete is not in the table any more, and because a
//! thousand subscribers must not cost a thousand queries per write.
//!
//! **Parity** with the database is what the tests hold it to (`tests/filters.rs`):
//! a matrix of filters × rows with nulls, the Rust verdict asserted equal to
//! "the REST read with this filter returns the row".
//!
//! ## Where backends differ, the backend decides
//!
//! `LIKE` is case-sensitive on Postgres, with `\` as its escape; SQLite's
//! `LIKE` folds ASCII case and has no escape, which is why its dialect renders
//! `ILIKE` as `LIKE`. [`LikeRules::of`] reads that off the dialect of the
//! database the table lives on, so the verdict is the one that database would
//! give. Text is ordered by bytes — the `C` collation; a database collating
//! text by a locale may order `a` and `B` the other way, so a range filter
//! over mixed-case text is the one place a verdict can differ.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use sc_catalog::Table;
use sc_db::DatabaseDriver;
use sc_error::{Error, Result};
use sc_query::{BinOp, Expr, InSet, UnOp, Value};
use sc_types::BasicType;
use serde_json::Value as Json;

/// How `LIKE` behaves on the database a table lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LikeRules {
    /// Whether plain `LIKE` already ignores (ASCII) case — SQLite's.
    pub like_folds_case: bool,
    /// Whether `\` escapes the next character of a pattern — Postgres's
    /// default.
    pub backslash_escapes: bool,
}

impl LikeRules {
    /// Postgres's rules: `LIKE` is case-sensitive, `ILIKE` folds case, and `\`
    /// escapes.
    pub const POSTGRES: LikeRules = LikeRules {
        like_folds_case: false,
        backslash_escapes: true,
    };

    /// SQLite's rules: `LIKE` folds ASCII case (so `ILIKE` is rendered as
    /// it), and nothing escapes.
    pub const SQLITE: LikeRules = LikeRules {
        like_folds_case: true,
        backslash_escapes: false,
    };

    /// The rules of the database `driver` talks to, read off its dialect: a
    /// dialect that renders `ILIKE` as `LIKE` has a `LIKE` that already folds
    /// case — SQLite's.
    pub fn of(driver: &dyn DatabaseDriver) -> LikeRules {
        let dialect = driver.dialect();
        if dialect.binary_op(BinOp::ILike) == dialect.binary_op(BinOp::Like) {
            LikeRules::SQLITE
        } else {
            LikeRules::POSTGRES
        }
    }
}

/// A subscription's filter, lowered and checked.
#[derive(Debug, Clone, PartialEq)]
pub struct RowFilter {
    /// The object as the client sent it, in canonical form — what two
    /// subscriptions with the same filter share a verdict by.
    key: String,
    /// The predicate, or `None` for "every row".
    expr: Option<Expr>,
    like: LikeRules,
}

impl RowFilter {
    /// Every row.
    pub fn all() -> RowFilter {
        RowFilter {
            key: String::new(),
            expr: None,
            like: LikeRules::POSTGRES,
        }
    }

    /// Lower `filter` against `table` as the REST read would, and check that
    /// every part of it can be evaluated here. An `Err` is the `invalid` the
    /// subscriber is told, naming what was wrong.
    pub fn parse(table: &Table, filter: Option<&Json>, like: LikeRules) -> Result<RowFilter> {
        let expr = sc_api::filter::api_where(table, filter)?;
        if let Some(expr) = &expr {
            check(table, expr)?;
        }
        Ok(RowFilter {
            key: match (&expr, filter) {
                (Some(_), Some(filter)) => canonical(filter).to_string(),
                _ => String::new(),
            },
            expr,
            like,
        })
    }

    /// What subscriptions with the same filter share a verdict by.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Whether the row `values` matches: the predicate is **true** for it
    /// (unknown is not a match, as a `WHERE` reads it).
    pub fn matches(&self, values: &BTreeMap<String, Value>) -> bool {
        match &self.expr {
            None => true,
            Some(expr) => eval(expr, values, self.like) == Some(true),
        }
    }
}

/// `value` with every object's keys in order, so two spellings of one filter
/// are one text (this workspace's `serde_json` keeps keys as they arrived).
fn canonical(value: &Json) -> Json {
    match value {
        Json::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Json::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), canonical(&map[key])))
                    .collect(),
            )
        }
        Json::Array(items) => Json::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// The basic type a column is stored as.
fn storage(table: &Table, column: &str) -> Option<BasicType> {
    let field = table.field(column)?;
    let type_ = &field.base.type_;
    Some(
        type_
            .as_basic()
            .cloned()
            .unwrap_or_else(|| BasicType::from_sql_type(type_.sql_type())),
    )
}

/// Refuse a predicate this module cannot evaluate. Everything the filter object
/// lowers to is evaluable; what is refused is a `like`/`ilike` on a column that
/// is not text, which the database would refuse too (there is no `LIKE` on an
/// integer).
fn check(table: &Table, expr: &Expr) -> Result<()> {
    match expr {
        Expr::Col(_) | Expr::Lit(_) => Ok(()),
        Expr::Binary { op, l, r } => {
            if matches!(op, BinOp::Like | BinOp::ILike)
                && let Expr::Col(column) = l.as_ref()
                && storage(table, &column.column) != Some(BasicType::Text)
            {
                return Err(Error::invalid(format!(
                    "`{}` is not text, so it cannot be matched against a pattern",
                    column.column
                )));
            }
            match op {
                BinOp::Eq
                | BinOp::Ne
                | BinOp::Lt
                | BinOp::Le
                | BinOp::Gt
                | BinOp::Ge
                | BinOp::And
                | BinOp::Or
                | BinOp::Like
                | BinOp::ILike => {
                    check(table, l)?;
                    check(table, r)
                }
                other => Err(unsupported(&format!("{other:?}"))),
            }
        }
        Expr::Unary { op, e } => match op {
            UnOp::Not | UnOp::IsNull | UnOp::IsNotNull => check(table, e),
            other => Err(unsupported(&format!("{other:?}"))),
        },
        Expr::In {
            e,
            set: InSet::List(items),
        } => {
            check(table, e)?;
            items.iter().try_for_each(|item| check(table, item))
        }
        other => Err(unsupported(&format!("{other:?}"))),
    }
}

fn unsupported(what: &str) -> Error {
    Error::invalid(format!(
        "this filter uses `{what}`, which a live subscription cannot evaluate"
    ))
}

/// Evaluate `expr` for a row, three-valued: `None` is SQL's unknown.
fn eval(expr: &Expr, values: &BTreeMap<String, Value>, like: LikeRules) -> Option<bool> {
    match expr {
        Expr::Binary { op, l, r } => match op {
            BinOp::And => match (eval(l, values, like), eval(r, values, like)) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            },
            BinOp::Or => match (eval(l, values, like), eval(r, values, like)) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            },
            BinOp::Like | BinOp::ILike => {
                let (Value::Text(text), Value::Text(pattern)) =
                    (operand(l, values)?, operand(r, values)?)
                else {
                    return None;
                };
                let fold = *op == BinOp::ILike || like.like_folds_case;
                Some(like_match(&text, &pattern, fold, like))
            }
            op => {
                let (a, b) = (operand(l, values)?, operand(r, values)?);
                // A JSON document equals another with the same content, and has
                // no order a filter could mean.
                if let (Value::Json(x), Value::Json(y)) = (&a, &b) {
                    return match op {
                        BinOp::Eq => Some(x == y),
                        BinOp::Ne => Some(x != y),
                        _ => None,
                    };
                }
                let ordering = compare(&a, &b);
                match op {
                    BinOp::Eq => ordering.map(Ordering::is_eq),
                    BinOp::Ne => ordering.map(Ordering::is_ne),
                    BinOp::Lt => ordering.map(Ordering::is_lt),
                    BinOp::Le => ordering.map(Ordering::is_le),
                    BinOp::Gt => ordering.map(Ordering::is_gt),
                    BinOp::Ge => ordering.map(Ordering::is_ge),
                    _ => None,
                }
            }
        },
        Expr::Unary { op, e } => match op {
            UnOp::Not => eval(e, values, like).map(|b| !b),
            UnOp::IsNull => Some(operand(e, values).is_none()),
            UnOp::IsNotNull => Some(operand(e, values).is_some()),
            _ => None,
        },
        Expr::In {
            e,
            set: InSet::List(items),
        } => {
            // SQL's `IN`: true if any item equals, otherwise unknown if the
            // value or any item was null, otherwise false.
            let value = operand(e, values)?;
            let mut unknown = false;
            for item in items {
                match operand(item, values) {
                    Some(candidate) => match compare(&value, &candidate) {
                        Some(Ordering::Equal) => return Some(true),
                        Some(_) => {}
                        None => unknown = true,
                    },
                    None => unknown = true,
                }
            }
            if unknown { None } else { Some(false) }
        }
        // A bare column or literal in a boolean position: its own truth.
        other => match operand(other, values)? {
            Value::Bool(b) => Some(b),
            _ => None,
        },
    }
}

/// A column's or a literal's value — `None` for a null (or a column the row
/// does not carry, which a read would have returned as null).
fn operand(expr: &Expr, values: &BTreeMap<String, Value>) -> Option<Value> {
    let value = match expr {
        Expr::Col(column) => values.get(&column.column)?.clone(),
        Expr::Lit(value) => value.clone(),
        _ => return None,
    };
    (!value.is_null()).then_some(value)
}

/// SQL's comparison of two non-null values: `None` when they are of kinds
/// that do not compare (which a coerced literal beside its own column never
/// is).
fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    use Value::*;
    match (a, b) {
        (Int(x), Int(y)) => Some(x.cmp(y)),
        (Float(x), Float(y)) => x.partial_cmp(y),
        (Decimal(x), Decimal(y)) => Some(x.cmp(y)),
        (Int(_) | Float(_) | Decimal(_), Int(_) | Float(_) | Decimal(_)) => {
            number(a)?.partial_cmp(&number(b)?)
        }
        (Text(x), Text(y)) => Some(x.as_bytes().cmp(y.as_bytes())),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (Uuid(x), Uuid(y)) => Some(x.cmp(y)),
        (Date(x), Date(y)) => Some(x.cmp(y)),
        (Time(x), Time(y)) => Some(x.cmp(y)),
        (Timestamp(x), Timestamp(y)) => Some(x.cmp(y)),
        (Bytes(x), Bytes(y)) => Some(x.cmp(y)),
        // A uuid column read back as text on a backend without the type.
        (Uuid(x), Text(y)) | (Text(y), Uuid(x)) => {
            let ordering = x.hyphenated().to_string().as_bytes().cmp(y.as_bytes());
            Some(if matches!(a, Uuid(_)) {
                ordering
            } else {
                ordering.reverse()
            })
        }
        _ => None,
    }
}

/// A number as `f64`, for comparing two of different kinds.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Int(n) => Some(*n as f64),
        Value::Float(f) => Some(*f),
        Value::Decimal(d) => d.to_string().parse().ok(),
        _ => None,
    }
}

/// One element of a `LIKE` pattern.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Piece {
    /// `%`: any run of characters, the empty one included.
    Any,
    /// `_`: exactly one character.
    One,
    /// A character to match.
    Char(char),
}

/// Whether `text` matches the `LIKE` pattern `pattern`, folding case when
/// `fold` (Unicode on Postgres's `ILIKE`, ASCII on SQLite's `LIKE`).
fn like_match(text: &str, pattern: &str, fold: bool, rules: LikeRules) -> bool {
    let fold_char = |c: char| -> char {
        if !fold {
            c
        } else if rules.like_folds_case {
            c.to_ascii_lowercase()
        } else {
            c.to_lowercase().next().unwrap_or(c)
        }
    };
    let mut pieces = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        pieces.push(match c {
            '%' => Piece::Any,
            '_' => Piece::One,
            '\\' if rules.backslash_escapes => Piece::Char(fold_char(chars.next().unwrap_or('\\'))),
            other => Piece::Char(fold_char(other)),
        });
    }
    let text: Vec<char> = text.chars().map(fold_char).collect();
    // The classic two-pointer wildcard match with backtracking to the last `%`.
    let (mut t, mut p) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pieces.get(p) {
            Some(Piece::Char(c)) if *c == text[t] => {
                t += 1;
                p += 1;
            }
            Some(Piece::One) => {
                t += 1;
                p += 1;
            }
            Some(Piece::Any) => {
                star = Some((p, t));
                p += 1;
            }
            _ => match star {
                Some((star_p, star_t)) => {
                    p = star_p + 1;
                    t = star_t + 1;
                    star = Some((star_p, star_t + 1));
                }
                None => return false,
            },
        }
    }
    pieces[p..].iter().all(|piece| *piece == Piece::Any)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use sc_catalog::{AccessRules, DataField, DbId, TableId, TableSource};
    use sc_types::{Attrs, TypeRef};
    use serde_json::json;

    pub(crate) fn table(name: &str, columns: &[(&str, BasicType)]) -> Table {
        Table {
            id: TableId(name.to_owned()),
            name: name.to_owned(),
            database: DbId::primary(),
            source: TableSource::Database,
            primary_key: vec!["id".to_owned()],
            fields: columns
                .iter()
                .map(|(name, ty)| DataField::plain(*name, TypeRef::Basic(ty.clone())))
                .collect(),
            label: name.to_owned(),
            description: String::new(),
            access: AccessRules::default(),
            attributes: Attrs::new(),
            overlay: None,
            ownership: None,
            ownership_error: None,
            rls_enabled: false,
            constraints: Vec::new(),
        }
    }

    fn cards() -> Table {
        table(
            "cards",
            &[
                ("id", BasicType::Int),
                ("board", BasicType::Int),
                ("title", BasicType::Text),
                ("position", BasicType::Float),
                ("due", BasicType::Date),
            ],
        )
    }

    fn row(json: Json) -> BTreeMap<String, Value> {
        sc_api::rows::row_json_values(&cards(), &json)
    }

    fn matches(filter: Json, row_json: Json) -> bool {
        RowFilter::parse(&cards(), Some(&filter), LikeRules::POSTGRES)
            .unwrap()
            .matches(&row(row_json))
    }

    #[test]
    fn an_absent_or_empty_filter_matches_every_row() {
        let table = cards();
        for filter in [None, Some(json!({})), Some(Json::Null)] {
            let parsed = RowFilter::parse(&table, filter.as_ref(), LikeRules::POSTGRES).unwrap();
            assert!(parsed.matches(&row(json!({ "id": 1, "board": null }))));
            assert_eq!(parsed.key(), "");
        }
    }

    #[test]
    fn equality_and_ranges_follow_sql_and_never_match_a_null() {
        let seven = json!({ "id": 1, "board": 7, "title": "a", "position": 1.5 });
        let unset = json!({ "id": 2, "board": null, "title": null });
        assert!(matches(json!({ "board": 7 }), seven.clone()));
        assert!(!matches(json!({ "board": 8 }), seven.clone()));
        assert!(!matches(json!({ "board": 7 }), unset.clone()));
        // `ne` is `<>`: a null is neither equal nor unequal.
        assert!(matches(json!({ "board": { "ne": 8 } }), seven.clone()));
        assert!(!matches(json!({ "board": { "ne": 8 } }), unset.clone()));
        // …and its negation is unknown too, so still not a match.
        assert!(!matches(json!({ "not": { "board": 7 } }), unset.clone()));
        // Null is asked about by name.
        assert!(matches(json!({ "board": null }), unset.clone()));
        assert!(matches(
            json!({ "board": { "is_null": true } }),
            unset.clone()
        ));
        assert!(matches(
            json!({ "board": { "is_null": false } }),
            seven.clone()
        ));
        assert!(matches(
            json!({ "position": { "gte": 1.5 } }),
            seven.clone()
        ));
        assert!(!matches(
            json!({ "position": { "gt": 1.5 } }),
            seven.clone()
        ));
        // An integer literal against a float column is coerced to the column.
        assert!(matches(json!({ "position": { "lt": 2 } }), seven));
    }

    #[test]
    fn in_and_or_follow_three_valued_logic() {
        let seven = json!({ "id": 1, "board": 7 });
        let unset = json!({ "id": 2, "board": null });
        assert!(matches(json!({ "board": { "in": [6, 7] } }), seven.clone()));
        assert!(!matches(
            json!({ "board": { "nin": [6, 7] } }),
            seven.clone()
        ));
        assert!(!matches(
            json!({ "board": { "in": [6, 7] } }),
            unset.clone()
        ));
        assert!(!matches(
            json!({ "board": { "nin": [6, 7] } }),
            unset.clone()
        ));
        // `true OR unknown` is true; `false OR unknown` is unknown.
        assert!(matches(
            json!({ "or": [{ "id": 2 }, { "board": 7 }] }),
            unset.clone()
        ));
        assert!(!matches(
            json!({ "or": [{ "id": 3 }, { "board": 7 }] }),
            unset
        ));
    }

    #[test]
    fn a_date_filter_compares_dates() {
        let row = json!({ "id": 1, "due": "2026-03-01" });
        assert!(matches(
            json!({ "due": { "lt": "2026-12-01" } }),
            row.clone()
        ));
        assert!(!matches(json!({ "due": { "lt": "2026-02-01" } }), row));
    }

    #[test]
    fn like_is_the_backends_like() {
        let row = row(json!({ "id": 1, "title": "Fix the 100% bug" }));
        let on = |filter: Json, rules: LikeRules| {
            RowFilter::parse(&cards(), Some(&filter), rules)
                .unwrap()
                .matches(&row)
        };
        assert!(on(
            json!({ "title": { "like": "Fix%" } }),
            LikeRules::POSTGRES
        ));
        assert!(!on(
            json!({ "title": { "like": "fix%" } }),
            LikeRules::POSTGRES
        ));
        assert!(on(
            json!({ "title": { "ilike": "fix%" } }),
            LikeRules::POSTGRES
        ));
        assert!(on(
            json!({ "title": { "like": "fix%" } }),
            LikeRules::SQLITE
        ));
        assert!(on(
            json!({ "title": { "like": "Fix _he%" } }),
            LikeRules::POSTGRES
        ));
        assert!(on(
            json!({ "title": { "like": "%100\\% bug" } }),
            LikeRules::POSTGRES
        ));
        assert!(!on(
            json!({ "title": { "like": "%1000\\%%" } }),
            LikeRules::POSTGRES
        ));
        assert!(!on(
            json!({ "title": { "like": "Fix" } }),
            LikeRules::POSTGRES
        ));
    }

    #[test]
    fn what_rest_refuses_is_refused_and_a_pattern_needs_text() {
        let table = cards();
        for (filter, says) in [
            (json!({ "colour": "red" }), "no field `colour`"),
            // Not an operator, so a literal to equal — which an integer column
            // refuses, as REST does.
            (json!({ "board": { "between": [1, 2] } }), "`board`"),
            (json!({ "board": { "like": "7%" } }), "not text"),
            (json!({ "due": "tomorrow" }), "`due`"),
        ] {
            let err = RowFilter::parse(&table, Some(&filter), LikeRules::POSTGRES)
                .unwrap_err()
                .to_string();
            assert!(err.contains(says), "{filter}: {err}");
        }
    }

    #[test]
    fn the_same_filter_has_the_same_key() {
        let table = cards();
        let a = RowFilter::parse(
            &table,
            Some(&json!({ "board": 7, "title": "x" })),
            LikeRules::POSTGRES,
        )
        .unwrap();
        let b = RowFilter::parse(
            &table,
            Some(&json!({ "title": "x", "board": 7 })),
            LikeRules::POSTGRES,
        )
        .unwrap();
        assert_eq!(a.key(), b.key());
    }
}
