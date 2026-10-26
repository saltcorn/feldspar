//! The read query string every list endpoint speaks (design §13.4).
//!
//! `?title=ilike.%rock%&order=published.desc&limit=50&offset=100` is one
//! vocabulary with two callers, so it is parsed once, here. The REST provider
//! reads it off an application's `GET /api/posts`; the admin API's `listRows`
//! and `countRows` read it off `/api/tables/{table}/rows`, which is what makes
//! the admin's own data grid — sorting a column, typing in its filter box,
//! scrolling to row 40,000 — cost one page of rows instead of the table.
//!
//! Everything the vocabulary *means* is decided elsewhere: a comparison lowers
//! through [`crate::filter`], shared with GraphQL, and every literal is coerced
//! against the column it stands beside rather than against JSON. This module is
//! only the **syntax** — how `gte.2020-01-01`, `published.desc` and `limit=50`
//! are spelled in a URL — and it parses into the row layer's own
//! [`RowQuery`], never into SQL text.
//!
//! `select` is the one reserved word this module does not act on: embeds are
//! the REST provider's own shape, so [`crate::rest::query`] keeps that half and
//! calls in here for the rest.

use sc_catalog::Table;
use sc_error::{Error, Result};
use sc_query::{Expr, OrderBy};
use serde_json::Value as Json;

use crate::filter;
use crate::rows::RowQuery;

/// The query-string keys that are not filters. A table column of the same name
/// cannot be filtered on — the same trade PostgREST makes, and the reason these
/// are four short reserved words rather than anything an application would
/// plausibly name a column.
pub const KEY_SELECT: &str = "select";
/// The ordering key (`order=published.desc,title`).
pub const KEY_ORDER: &str = "order";
/// The row bound (`limit=50`).
pub const KEY_LIMIT: &str = "limit";
/// How many rows to skip (`offset=100`).
pub const KEY_OFFSET: &str = "offset";
/// A full-text search (`search=rust -java`, TODO-agent-skills AS1.4).
pub const KEY_SEARCH: &str = "search";
/// Every reserved word, in the order the parser checks them.
pub const RESERVED: [&str; 5] = [KEY_SELECT, KEY_ORDER, KEY_LIMIT, KEY_OFFSET, KEY_SEARCH];

/// The filters, ordering and bounds of one query string, as a [`RowQuery`].
///
/// `pairs` is the query string in **arrival order with duplicates kept**, which
/// is what makes a range (`?published=gte.2020&published=lt.2024`) expressible:
/// a map would keep whichever came last and silently drop the other half of the
/// range, and a silently dropped filter is the worst failure a list endpoint can
/// have.
///
/// `row_cap` is the caller's ceiling: an absent `limit` becomes it and a present
/// one is **clamped** to it. A caller's number is a request, not a permission —
/// a list endpoint with no bound is how a table ends up streamed into a response
/// by accident.
///
/// A `search` narrows the rows to those matching it and, unless an `order` says
/// otherwise, orders them best match first; an `order` names that ordering
/// `relevance` (`order=relevance,title`).
pub fn row_query(table: &Table, pairs: &[(String, String)], row_cap: u64) -> Result<RowQuery> {
    let mut query = RowQuery::new().where_(column_filters(table, pairs)?);
    let search = match first(pairs, KEY_SEARCH) {
        Some(phrase) => Some(crate::search::text_search(table, phrase, None)?),
        None => None,
    };
    match (first(pairs, KEY_ORDER), &search) {
        (Some(order), _) => {
            query = query.order_by(order_keys_searching(table, order, search.as_ref())?);
        }
        (None, Some(search)) => query = query.order_by(vec![crate::search::relevance(search)]),
        (None, None) => {}
    }
    if let Some(search) = search {
        query = query.searching(search);
    }
    let limit = bound(first(pairs, KEY_LIMIT), KEY_LIMIT)?;
    query = query.limit(limit.map_or(row_cap, |n| n.min(row_cap)));
    if let Some(offset) = bound(first(pairs, KEY_OFFSET), KEY_OFFSET)? {
        query = query.offset(offset);
    }
    Ok(query)
}

/// The predicate a query string's filters AND together, or `None` when it has
/// none.
///
/// Separate from [`row_query`] because a **count** is the same question without
/// the ordering or the bound: the admin grid's scrollbar has to be as long as
/// the rows the filter row leaves, and counting them through a second spelling
/// of the same filters is how the two come to disagree.
///
/// A `search` is part of it: a count of a searched list counts the matches.
pub fn filter_predicate(table: &Table, pairs: &[(String, String)]) -> Result<Option<Expr>> {
    let filters = column_filters(table, pairs)?;
    let Some(phrase) = first(pairs, KEY_SEARCH) else {
        return Ok(filters);
    };
    let matched = Expr::TextMatch(Box::new(crate::search::text_search(table, phrase, None)?));
    Ok(Some(match filters {
        Some(filters) => filters.and(matched),
        None => matched,
    }))
}

/// The `column=op.value` filters, ANDed.
fn column_filters(table: &Table, pairs: &[(String, String)]) -> Result<Option<Expr>> {
    let mut predicate: Option<Expr> = None;
    for (key, value) in pairs {
        if RESERVED.contains(&key.as_str()) {
            continue;
        }
        let expr = filter_expr(table, key, value)?;
        predicate = Some(match predicate {
            Some(existing) => existing.and(expr),
            None => expr,
        });
    }
    Ok(predicate)
}

/// The first value given for a query-string key, or `None`.
fn first<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// One `column=op.value` filter as a predicate.
pub fn filter_expr(table: &Table, key: &str, spec: &str) -> Result<Expr> {
    if key.contains('.') {
        return Err(Error::invalid(format!(
            "`{key}` filters an embedded resource, which this API does not take: an embed is \
             a correlated subquery of this table's read, so it cannot decide which rows of \
             `{}` come back — filter the embedded table in a read of its own",
            table.name
        )));
    }
    if crate::user_rows::visible_field(table, key).is_none() {
        // `or=(…)`/`and=(…)`/`not.…` are PostgREST's, deliberately not taken
        // (see the module docs). They are named as the features they are —
        // unless the application really does have a column called `or`, in which
        // case the check above already accepted it as one.
        if matches!(key, "or" | "and" | "not") {
            return Err(Error::invalid(format!(
                "`{key}=` is a PostgREST boolean-logic parameter, which this API does not \
                 take: filters given here are ANDed together"
            )));
        }
        return Err(Error::invalid(format!(
            "`{}` has no field `{key}` to filter on",
            table.name
        )));
    }
    if sc_catalog::secrets::is_secret_column(table, key) {
        return Err(Error::invalid(format!(
            "`{}.{key}` is a secret; it cannot be filtered on",
            table.name
        )));
    }
    let Some((op, operand)) = spec.split_once('.') else {
        return Err(Error::invalid(format!(
            "`{key}={spec}` is not a filter: a filter is `column=op.value`, e.g. \
             `{key}=eq.{spec}` — the comparisons are {}",
            filter::OPERATORS.join(", ")
        )));
    };
    if op == "not" {
        return Err(Error::invalid(format!(
            "`{key}=not.…` is not taken: negate with `ne`, `nin`, or `is_null.false`"
        )));
    }
    filter::comparison(table, key, Expr::col(key), op, &operand_json(op, operand)?)
}

/// A filter's operand as the JSON the shared vocabulary consumes.
///
/// Everything on the wire is text, and it stays text: `rows::column_value`
/// coerces it against the *column*, so `gte.2020-01-01` on a Date column binds a
/// date and on a Text column binds the string. The two operators whose operand
/// is not a value of the column are the exceptions — a list, and a boolean.
fn operand_json(op: &str, operand: &str) -> Result<Json> {
    match op {
        "in" | "nin" => {
            let items = operand
                .strip_prefix('(')
                .and_then(|s| s.strip_suffix(')'))
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "`{op}` takes a parenthesised list, e.g. `{op}.(1,2,3)`"
                    ))
                })?;
            Ok(Json::Array(
                split_list(items).into_iter().map(Json::String).collect(),
            ))
        }
        "is_null" => match operand {
            "true" => Ok(Json::Bool(true)),
            "false" => Ok(Json::Bool(false)),
            other => Err(Error::invalid(format!(
                "`is_null` takes `true` or `false`, not `{other}`"
            ))),
        },
        _ => Ok(Json::String(operand.to_owned())),
    }
}

/// The elements of an `in.(…)` list: comma-separated, with double quotes around
/// any element that contains a comma of its own.
fn split_list(items: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in items.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    // An empty list (`in.()`) is a membership test against nothing, which is
    // legal and matches nothing; anything else ends with a final element.
    if !current.is_empty() || !out.is_empty() {
        out.push(current);
    }
    out
}

/// `order=published.desc,title` as the row layer's ordering keys.
pub fn order_keys(table: &Table, spec: &str) -> Result<Vec<OrderBy>> {
    order_keys_searching(table, spec, None)
}

/// [`order_keys`] on a read with a search, where `relevance` — unless the table
/// has a field of that name — is how well a row matches it.
fn order_keys_searching(
    table: &Table,
    spec: &str,
    search: Option<&sc_query::TextSearch>,
) -> Result<Vec<OrderBy>> {
    let mut keys = Vec::new();
    for item in spec.split(',').filter(|s| !s.trim().is_empty()) {
        let mut parts = item.trim().split('.');
        let column = parts.next().unwrap_or_default();
        let visible = crate::user_rows::visible_field(table, column).is_some();
        if column == crate::search::RELEVANCE && !visible {
            let Some(search) = search else {
                return Err(Error::invalid(format!(
                    "`order={column}` orders by how well a row matches a search, and this \
                     read has no `{KEY_SEARCH}`"
                )));
            };
            let rank = crate::search::relevance(search);
            keys.push(match parts.next() {
                None | Some("desc") => rank,
                Some("asc") => OrderBy::asc(rank.expr),
                Some(other) => {
                    return Err(Error::invalid(format!(
                        "`{column}` must be ordered `asc` or `desc`, not `{other}`"
                    )));
                }
            });
            continue;
        }
        if !visible {
            return Err(Error::invalid(format!(
                "`{}` has no field `{column}` to order by",
                table.name
            )));
        }
        if sc_catalog::secrets::is_secret_column(table, column) {
            return Err(Error::invalid(format!(
                "`{}.{column}` is a secret; it cannot be ordered by",
                table.name
            )));
        }
        let expr = Expr::col(column);
        keys.push(match parts.next() {
            None | Some("asc") => OrderBy::asc(expr),
            Some("desc") => OrderBy::desc(expr),
            Some(other) => {
                return Err(Error::invalid(format!(
                    "`{}`.`{column}` must be ordered `asc` or `desc`, not `{other}`",
                    table.name
                )));
            }
        });
        if let Some(more) = parts.next() {
            return Err(Error::invalid(format!(
                "`{more}` is not part of an `order` key: `column.asc`/`column.desc` is the \
                 whole of it"
            )));
        }
    }
    Ok(keys)
}

/// A `limit`/`offset` value: absent is `None`, anything that is not a whole
/// number is refused naming both the parameter and what arrived.
pub fn bound(value: Option<&str>, key: &str) -> Result<Option<u64>> {
    match value {
        None => Ok(None),
        Some(text) => text.trim().parse::<u64>().map(Some).map_err(|_| {
            Error::invalid(format!(
                "`{key}` must be a whole number of rows, not `{text}`"
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphql::testing::{id_field, key_field, plain_field, table_of, typed_field};
    use sc_query::{OrderDir, SqlDialect, Value};
    use sc_types::BasicType;

    struct Pg;

    impl SqlDialect for Pg {
        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
        fn placeholder(&self, position: usize) -> String {
            format!("${position}")
        }
    }
    fn books() -> Table {
        table_of(
            "books",
            vec![
                id_field(),
                plain_field("title"),
                typed_field("pages", BasicType::Int),
                typed_field("published", BasicType::Date),
                key_field("author", "authors", "id"),
            ],
        )
    }

    /// The query string as this module reads it: pairs in arrival order, with
    /// duplicates kept.
    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// The filter half of a query string, rendered — no catalog needed, because
    /// a filter never reaches one.
    fn where_of(items: &[(&str, &str)]) -> Result<(String, Vec<Value>)> {
        let table = books();
        let predicate = filter_predicate(&table, &pairs(items))?;
        let stmt: sc_query::Statement = sc_query::Select::from(sc_query::Source::table("books"))
            .filter(predicate.expect("a predicate"))
            .into();
        let (sql, binds) = Pg.render(&stmt).expect("renders");
        Ok((
            sql.split_once(" WHERE ")
                .map(|(_, w)| w.to_owned())
                .unwrap_or_else(|| panic!("no WHERE in {sql}")),
            binds,
        ))
    }

    fn error_of(items: &[(&str, &str)]) -> String {
        format!(
            "{}",
            where_of(items).expect_err("this query string is refused")
        )
    }

    #[test]
    fn a_filter_lowers_to_the_same_expr_the_graphql_syntax_does() {
        // Decision 1, asserted: `?published=gte.2020-01-01` and
        // `where: { published: { gte: "2020-01-01" } }` are two spellings of one
        // question, and they reach the row layer as the same value — so the date
        // is bound as a *date* in both, by construction rather than by two
        // implementations that happen to agree today.
        let table = books();
        let rest = filter_expr(&table, "published", "gte.2020-01-01").expect("parses");
        let graphql = crate::graphql::args::where_expr(
            &table,
            &async_graphql::Value::from_json(serde_json::json!({
                "published": { "gte": "2020-01-01" }
            }))
            .expect("a GraphQL value"),
            None,
        )
        .expect("lowers")
        .expect("a predicate");
        assert_eq!(rest, graphql);
    }

    #[test]
    fn a_repeated_key_is_a_range_and_both_halves_survive() {
        // The reason `ApiRequest.query` is a list of pairs: a map would keep the
        // second bound and quietly return everything before 2020 as well.
        let (sql, binds) = where_of(&[
            ("published", "gte.2020-01-01"),
            ("published", "lt.2024-01-01"),
        ])
        .expect("parses");
        assert_eq!(sql, "((\"published\" >= $1) AND (\"published\" < $2))");
        assert!(
            matches!(binds.as_slice(), [Value::Date(_), Value::Date(_)]),
            "expected two bound dates, got {binds:?}"
        );
    }

    #[test]
    fn the_wire_is_text_and_the_column_decides_what_it_means() {
        let (sql, binds) = where_of(&[("pages", "gt.100")]).expect("parses");
        assert_eq!(sql, "(\"pages\" > $1)");
        assert_eq!(binds, vec![Value::Int(100)]);
    }

    #[test]
    fn in_takes_a_parenthesised_list_and_is_null_takes_a_boolean() {
        let (sql, binds) = where_of(&[("pages", "in.(100,200)")]).expect("parses");
        assert_eq!(sql, "(\"pages\" IN ($1, $2))");
        assert_eq!(binds, vec![Value::Int(100), Value::Int(200)]);

        let (sql, binds) = where_of(&[("title", "is_null.false")]).expect("parses");
        assert_eq!(sql, "(\"title\" IS NOT NULL)");
        assert!(binds.is_empty());

        // A quoted element keeps its comma.
        let (_, binds) = where_of(&[("title", "in.(\"a,b\",c)")]).expect("parses");
        assert_eq!(
            binds,
            vec![Value::Text("a,b".into()), Value::Text("c".into())]
        );
    }

    #[test]
    fn a_value_that_looks_like_sql_is_a_bound_value_and_nothing_else() {
        let (sql, binds) = where_of(&[("title", "eq.'; drop table books; --")]).expect("parses");
        assert_eq!(sql, "(\"title\" = $1)");
        assert_eq!(binds, vec![Value::Text("'; drop table books; --".into())]);
    }

    #[test]
    fn an_unknown_column_an_unknown_operator_and_a_shapeless_filter_are_each_refused_by_name() {
        assert!(
            error_of(&[("nope", "eq.1")]).contains("has no field `nope`"),
            "{}",
            error_of(&[("nope", "eq.1")])
        );
        let unknown_op = error_of(&[("pages", "between.1")]);
        assert!(
            unknown_op.contains("`between` is not a comparison"),
            "{unknown_op}"
        );
        let shapeless = error_of(&[("pages", "100")]);
        assert!(
            shapeless.contains("`pages=100` is not a filter"),
            "{shapeless}"
        );
    }

    #[test]
    fn the_not_taken_filter_grammar_is_refused_by_the_name_it_is_known_by() {
        // Decision 2: never ignored. A dropped filter is rows the caller did not
        // ask for, so each of these says what it is rather than 400-ing vaguely.
        let or = error_of(&[("or", "(pages.gt.100,title.eq.x)")]);
        assert!(or.contains("boolean-logic parameter"), "{or}");
        let not = error_of(&[("pages", "not.eq.1")]);
        assert!(not.contains("`pages=not.…` is not taken"), "{not}");
        let embedded = error_of(&[("author.name", "eq.Rowling")]);
        assert!(
            embedded.contains("filters an embedded resource"),
            "{embedded}"
        );
    }

    #[test]
    fn order_takes_a_precedence_list_and_defaults_to_ascending() {
        let keys = order_keys(&books(), "published.desc,title").expect("parses");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].dir, OrderDir::Desc);
        assert_eq!(keys[0].expr, Expr::col("published"));
        assert_eq!(keys[1].dir, OrderDir::Asc);

        let unknown = order_keys(&books(), "nope.asc").unwrap_err().to_string();
        assert!(unknown.contains("has no field `nope`"), "{unknown}");
        // PostgREST's `nullsfirst` is not taken, and says so rather than being
        // read as a direction nobody asked for.
        let nulls = order_keys(&books(), "title.nullsfirst")
            .unwrap_err()
            .to_string();
        assert!(nulls.contains("`nullsfirst`"), "{nulls}");
    }

    #[test]
    fn a_bound_is_a_whole_number_or_a_refusal_naming_it() {
        assert_eq!(bound(Some("20"), KEY_LIMIT).expect("parses"), Some(20));
        assert_eq!(bound(None, KEY_LIMIT).expect("parses"), None);
        let err = bound(Some("lots"), KEY_LIMIT).unwrap_err().to_string();
        assert!(err.contains("`limit` must be a whole number"), "{err}");
        // A negative bound is not a number of rows either.
        assert!(bound(Some("-1"), KEY_OFFSET).is_err());
    }

    #[test]
    fn a_whole_query_string_is_a_filter_an_order_and_a_bound_at_once() {
        // What the admin grid sends when a column is sorted and its filter box
        // typed in while the viewport sits at row 100: one read, bounded.
        let table = books();
        let query = row_query(
            &table,
            &pairs(&[
                ("title", "ilike.%rock%"),
                ("order", "published.desc"),
                ("limit", "50"),
                ("offset", "100"),
            ]),
            1000,
        )
        .expect("parses");
        assert!(query.filter.is_some());
        assert_eq!(query.order.len(), 1);
        assert_eq!(query.order[0].dir, OrderDir::Desc);
        assert_eq!(query.limit, Some(50));
        assert_eq!(query.offset, Some(100));
    }

    #[test]
    fn the_cap_is_the_default_and_the_ceiling_of_limit() {
        let table = books();
        // Absent: the cap. A caller who asks for more than the cap gets the cap
        // — the number is a request, not a permission.
        assert_eq!(
            row_query(&table, &[], 200).expect("parses").limit,
            Some(200)
        );
        assert_eq!(
            row_query(&table, &pairs(&[("limit", "5000")]), 200)
                .expect("parses")
                .limit,
            Some(200)
        );
    }

    #[test]
    fn a_reserved_word_is_not_read_as_a_filter() {
        // `order`/`limit`/`offset`/`select` are the vocabulary's own keys, so a
        // query string carrying them alone filters nothing rather than being
        // refused for having no field called `limit`.
        let table = books();
        assert!(
            filter_predicate(
                &table,
                &pairs(&[("order", "title"), ("limit", "10"), ("select", "title")])
            )
            .expect("parses")
            .is_none()
        );
    }
}
